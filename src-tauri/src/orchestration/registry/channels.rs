//! Cross-workspace channels (#271): the human-built membership graph that
//! connects agent panes across groups (`connect_agents`, `disconnect_agent`,
//! `set_sender`), and the agent-facing send and status over it
//! (`channel_send`, `channel_status`), as an `impl OrchRegistry` block
//! (#3498). The design is `docs/design/cross-workspace-channel.md`.

use super::*;

impl OrchRegistry {
    // ---------- cross-workspace channels (#271): human-connected agent-pane sessions ----------
    //
    // A "workspace" is a project tab, and each tab owns at most one
    // orchestration group — so "cross-workspace" is cross-group inside this
    // one process, one registry (`docs/design/cross-workspace-channel.md`).
    // A channel is shared in-memory state, mirroring `watches`; a message is
    // delivered through the SAME `deliver_prompt` visible-prompt path every
    // other agent-to-pane delivery uses (`report`, `send_prompt`, a fired
    // watch notice) — no new transport, no polling.
    //
    // The trust boundary: connect/disconnect are human-only, reached ONLY
    // from Tauri commands (CLAUDE.md constraint 5) — no MCP tool ever
    // mutates membership. `channel_send` (MCP, agent-facing) takes ONLY
    // `text`; the caller's peers are resolved from the membership graph a
    // human built, so an agent can reach exactly the panes a human connected
    // it to and nothing else — the membership graph IS the capability
    // (constraint 6). Every crossing text is scrubbed with
    // `notify::sanitize_gh_text` before it ever reaches a peer's pane, and
    // the identity line prefixed to it is built by loomux from the CALLER's
    // own backend-resolved identity, never from agent-supplied text — a peer
    // cannot forge who a message is from.

    /// Cap on one `channel_send` message before/at sanitization — generous
    /// enough for a real cross-workspace status update, small enough that an
    /// agent can't stash an unbounded blob in a single call.
    const CHANNEL_TEXT_CAP: usize = 2000;

    /// Build the identity line loomux prefixes to every crossing message and
    /// every connect notice — name, role, and repo, so whoever reads either
    /// pane knows who's on the other end and which workspace they're in.
    /// Built entirely from backend-resolved state, never agent-supplied text,
    /// so it can never be forged by a peer. Sanitized with the same
    /// `sanitize_gh_text` `text` gets: `m.name` is capped but not
    /// bracket-neutralized at the source (`sanitize_agent_name` keeps
    /// `[`/`]`), and an orchestrator-set name is not `channel_send`-callable
    /// text — low risk, not a forgery — but this keeps the design note's
    /// "can never be forged" literally true of the whole delivered line, not
    /// just the caller-supplied part of it.
    fn channel_member_label(&self, m: &ChannelMember) -> String {
        let repo = self.group(&m.group).map(|g| g.repo).unwrap_or_default();
        notify::sanitize_gh_text(&format!("{} ({}, {repo})", m.name, m.role.as_str()), 200)
    }

    /// Whether `agent_id` holds an MCP token — the sole gate on the sender
    /// role (#271 W3 addendum, part B6: "sender requires a token"). An
    /// unresolvable agent (dead/vanished) reads as no-token, never a panic.
    fn agent_has_token(&self, agent_id: &str) -> bool {
        self.agent(agent_id).is_some_and(|a| !a.token.is_empty())
    }

    /// `members`, plus (given the channel's `sender_id`) each member's
    /// directional standing:
    /// - `direction` ("sender"|"receiver").
    /// - `can_send`: whether it may `channel_send` RIGHT NOW — always true
    ///   for the sender; for a receiver, only while it holds the reply
    ///   credit AND has a token.
    /// - `delivery_only`: the STRUCTURAL fact (no token, full stop) rather
    ///   than the momentary one — a delivery-only member's `can_send` is
    ///   always false, but so is a normal receiver's between messages, and
    ///   the UI (#271 W3 addendum's A4 "represented honestly everywhere")
    ///   needs to tell "will never be able to reply" (permanent) apart from
    ///   "can't reply yet" (temporary) — the "receive-only" chip variant vs.
    ///   the plain receiver one.
    fn channel_members_json(&self, sender_id: &str, members: &[ChannelMember]) -> Vec<Value> {
        members
            .iter()
            .map(|m| {
                let is_sender = m.agent_id == sender_id;
                let has_token = self.agent_has_token(&m.agent_id);
                let can_send = is_sender || (m.may_reply && has_token);
                json!({
                    "group": m.group, "agent_id": m.agent_id, "name": m.name, "role": m.role,
                    "direction": if is_sender { "sender" } else { "receiver" },
                    "can_send": can_send,
                    "delivery_only": !has_token,
                })
            })
            .collect()
    }

    /// Lowest positive integer not currently used as a `display_number` by
    /// any live channel (#271 follow-up) — chan-1 closing frees up "1" for
    /// the next mint; with actives `{1, 3}` the next mint gets `2`, not `4`.
    /// Called while holding `channels`'s lock (the caller already does, to
    /// insert the new `Channel`), so this can't race a concurrent mint into
    /// picking the same number twice. O(n log n) over currently-live
    /// channels — a human-driven count, never large enough to matter.
    fn next_display_number(channels: &HashMap<String, Channel>) -> u32 {
        let mut used: Vec<u32> = channels.values().map(|c| c.display_number).collect();
        used.sort_unstable();
        let mut candidate = 1;
        for n in used {
            if n == candidate {
                candidate += 1;
            } else if n > candidate {
                break;
            }
        }
        candidate
    }

    /// Human-only (CLAUDE.md constraint 5 — called from a Tauri command,
    /// never reachable from MCP): connect two agent panes into a channel.
    /// - both free → mints a new channel with both as members.
    /// - one free, one already connected → the free pane JOINS the connected
    ///   pane's channel (multi-party): "a pane talking to two peers" is a
    ///   3-member channel, not a pane in two channels.
    /// - both already connected to the SAME channel → idempotent no-op.
    /// - both already connected to DIFFERENT channels → rejected: joining
    ///   would silently bridge two otherwise-isolated sessions through the
    ///   shared pane — exactly the uncontrolled cross-talk this feature
    ///   exists to bound (the one-channel-per-pane invariant, see the design
    ///   note's Membership section).
    /// Rejects planners (a planner's pane closes the instant it reports done,
    /// #203 — a member that can vanish mid-session is a liability) and
    /// dead/unknown/stale agent references.
    ///
    /// `sender_agent` (#271 W3 addendum, part B) means something different
    /// depending on whether this is a MINT or a JOIN — review round 2's B1
    /// finding was exactly this ambiguity, so it is spelled out fully here:
    ///
    /// - **MINT** (neither `from_agent` nor `to_agent` is already
    ///   connected): `sender_agent` **designates** the new channel's sender.
    ///   It must be one of the two named panes, and that pane must hold a
    ///   token (a delivery-only pane can never be the sender).
    /// - **JOIN** (either side is already connected): the channel's sender
    ///   already exists. `sender_agent` here does not designate anything —
    ///   it **confirms** who that already-fixed sender is. The confirmed
    ///   party is frequently **neither** `from_agent` nor `to_agent`: the
    ///   completion gesture can land on any member of the existing channel,
    ///   including a plain receiver, in which case the true sender is a
    ///   third identity not named by this call at all (e.g. a 4-member star
    ///   where a newcomer completes onto a receiver — the sender is the
    ///   fourth pane). Requiring the confirmation to equal
    ///   `Channel.sender` — never requiring it to be `from_agent`/
    ///   `to_agent` — is what makes B4's "a join can never reassign the
    ///   sender" invariant hold while still letting the human complete the
    ///   gesture on ANY existing member, not only the sender itself.
    pub fn connect_agents(
        &self,
        from_group: &GroupId,
        from_agent: &str,
        to_group: &GroupId,
        to_agent: &str,
        sender_agent: &str,
    ) -> Result<Value, String> {
        if from_agent == to_agent {
            return Err("cannot connect a pane to itself".into());
        }
        let a = self.agent(from_agent).ok_or("unknown agent: from_agent")?;
        let b = self.agent(to_agent).ok_or("unknown agent: to_agent")?;
        if a.group != from_group || b.group != to_group {
            return Err("stale pane reference — reload and try again".into());
        }
        if a.status == AgentStatus::Dead || b.status == AgentStatus::Dead {
            return Err("cannot connect a dead pane".into());
        }
        if a.role == Role::Planner || b.role == Role::Planner {
            return Err("a planner's pane closes the instant it reports done (#203) — \
                         planners can never join a channel"
                .into());
        }
        // #1161 M2: a manager can never join a channel either, and for a
        // different reason worth stating rather than folding into the arm
        // above. A planner is excluded because its pane will not be there;
        // a manager's pane will be there and is exactly the problem —
        // `channel_send` DELIVERS, and delivery into the human's own
        // conversation is the one thing this feature forbids.
        //
        // `deliver_prompt` would refuse the send anyway, so this is not what
        // makes the guarantee hold. It is what makes the refusal legible: a
        // human connecting two panes learns now, at the gesture, instead of
        // watching every later message vanish into an audit line.
        if a.role == Role::Manager || b.role == Role::Manager {
            return Err("a manager's pane is the human's own conversation, and it takes no \
                         delivery from any agent (#1161) — a manager can never join a channel. The \
                         orchestrator reaches it with message_manager instead."
                .into());
        }

        let mut channels = self.channels.lock_safe();
        let mut agent_channel = self.agent_channel.lock_safe();
        let a_chan = agent_channel.get(from_agent).cloned();
        let b_chan = agent_channel.get(to_agent).cloned();

        // A JOIN (one or both sides already connected) must agree with the
        // existing channel's sender — the star topology has exactly one hub,
        // and a second designation here is the same conflict `set_sender`
        // guards against, just reached through connect instead of a swap.
        // Deliberately does NOT require `sender_agent` to be `from_agent`/
        // `to_agent` — see the doc comment above.
        let require_matching_sender = |channels: &HashMap<String, Channel>, existing: &str| -> Result<(), String> {
            let ch = channels.get(existing).expect("agent_channel/channels must agree");
            if ch.sender != sender_agent {
                return Err(format!(
                    "channel {existing} already has a sender ({}) — swap it first (set_sender) \
                     before connecting with a different one",
                    ch.sender
                ));
            }
            Ok(())
        };

        let chan_id = match (a_chan, b_chan) {
            (Some(x), Some(y)) if x == y => {
                require_matching_sender(&channels, &x)?;
                x // already connected to each other
            }
            (Some(_), Some(_)) => {
                drop(agent_channel);
                drop(channels);
                return Err(
                    "both panes are already connected — to different channels; \
                     disconnect one first"
                        .into(),
                );
            }
            (Some(existing), None) => {
                require_matching_sender(&channels, &existing)?;
                let ch = channels.get_mut(&existing).expect("agent_channel/channels must agree");
                ch.members.push(ChannelMember {
                    group: b.group.clone(),
                    agent_id: b.id.clone(),
                    name: b.name.clone(),
                    role: b.role,
                    may_reply: false,
                });
                agent_channel.insert(to_agent.to_string(), existing.clone());
                existing
            }
            (None, Some(existing)) => {
                require_matching_sender(&channels, &existing)?;
                let ch = channels.get_mut(&existing).expect("agent_channel/channels must agree");
                ch.members.push(ChannelMember {
                    group: a.group.clone(),
                    agent_id: a.id.clone(),
                    name: a.name.clone(),
                    role: a.role,
                    may_reply: false,
                });
                agent_channel.insert(from_agent.to_string(), existing.clone());
                existing
            }
            (None, None) => {
                // MINT ONLY: here — and only here — `sender_agent` must
                // actually be one of the two named panes, and must hold a
                // token. A join never reaches this arm.
                if sender_agent != from_agent && sender_agent != to_agent {
                    drop(agent_channel);
                    drop(channels);
                    return Err("sender_agent must be one of the two connected panes".into());
                }
                let designated = if sender_agent == from_agent { &a } else { &b };
                if designated.token.is_empty() {
                    drop(agent_channel);
                    drop(channels);
                    return Err("a receive-only pane can't be the sender — it has no token".into());
                }
                let seq = self.channel_seq.fetch_add(1, Ordering::Relaxed) + 1;
                let id = format!("chan-{seq}");
                let display_number = Self::next_display_number(&channels);
                let ch = Channel {
                    id: id.clone(),
                    created_ms: now_ms(),
                    sender: sender_agent.to_string(),
                    display_number,
                    members: vec![
                        ChannelMember {
                            group: a.group.clone(),
                            agent_id: a.id.clone(),
                            name: a.name.clone(),
                            role: a.role,
                            may_reply: false,
                        },
                        ChannelMember {
                            group: b.group.clone(),
                            agent_id: b.id.clone(),
                            name: b.name.clone(),
                            role: b.role,
                            may_reply: false,
                        },
                    ],
                };
                channels.insert(id.clone(), ch);
                agent_channel.insert(from_agent.to_string(), id.clone());
                agent_channel.insert(to_agent.to_string(), id.clone());
                id
            }
        };
        let ch = channels.get(&chan_id).expect("just inserted/updated").clone();
        drop(agent_channel);
        drop(channels);

        let member_json = self.channel_members_json(&ch.sender, &ch.members);
        self.audit(&a.group, "human", "channel-connect",
            json!({ "channel_id": ch.id, "members": member_json, "sender": ch.sender }));
        if b.group != a.group {
            self.audit(&b.group, "human", "channel-connect",
                json!({ "channel_id": ch.id, "members": member_json, "sender": ch.sender }));
        }
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "orch-channel",
                channel_connected_event(&ch.id, &ch.sender, ch.display_number, member_json.clone()),
            );
        }
        let a_label = self.channel_member_label(&ChannelMember {
            group: a.group.clone(),
            agent_id: a.id.clone(),
            name: a.name.clone(),
            role: a.role,
            may_reply: false,
        });
        let b_label = self.channel_member_label(&ChannelMember {
            group: b.group.clone(),
            agent_id: b.id.clone(),
            name: b.name.clone(),
            role: b.role,
            may_reply: false,
        });
        let direction_note = |am_i_sender: bool, peer_label: &str| -> String {
            if am_i_sender {
                format!(
                    "[orrerix] connected to {peer_label} in channel {} — you are the SENDER: use \
                     channel_send(text) any time to reach them; channel_status() lists everyone \
                     connected.",
                    ch.id
                )
            } else {
                format!(
                    "[orrerix] connected to {peer_label} in channel {} — you are a RECEIVER: \
                     channel_send(text) works once {peer_label} messages you (reply-only); \
                     channel_status() lists everyone connected.",
                    ch.id
                )
            }
        };
        let _ = self.deliver_prompt(&a.id, &direction_note(ch.sender == a.id, &b_label), brand::AUDIT_ACTOR, Delivery::MidSession);
        let _ = self.deliver_prompt(&b.id, &direction_note(ch.sender == b.id, &a_label), brand::AUDIT_ACTOR, Delivery::MidSession);

        // Same shape as `channel_list`/`channel_for_pane` (`id`, `created_ms`,
        // `members`) — the frozen `OrchChannel` contract the UI slice builds
        // against. This is the command's RETURN VALUE, distinct from the
        // `channel-connect` audit record and the `orch-channel` event above,
        // both of which key the id `channel_id` on purpose (matching
        // `ChannelDisconnectResult`/`OrchChannelEvent`) — only this value
        // must match `OrchChannel`.
        Ok(json!({
            "id": ch.id, "created_ms": ch.created_ms, "sender": ch.sender,
            "display_number": ch.display_number, "members": member_json,
        }))
    }

    /// Human-only (constraint 5): remove `agent` from its channel. If
    /// membership drops below 2, OR `agent` was the channel's sender, the
    /// channel is torn down entirely and every stranded remaining member is
    /// notified and audited. The sender-loss case is additive to #285: a
    /// star topology has exactly one hub — losing it leaves receivers that
    /// can never initiate and each other that can never be reached (B4:
    /// receiver→receiver is never allowed), so the channel is as dead as a
    /// 1-member one. There is no automatic promotion; a human must
    /// `set_sender` and (if desired) reconnect.
    pub fn disconnect_agent(&self, group: &GroupId, agent: &str) -> Result<Value, String> {
        let mut channels = self.channels.lock_safe();
        let mut agent_channel = self.agent_channel.lock_safe();
        let chan_id = agent_channel
            .remove(agent)
            .ok_or_else(|| format!("{agent} is not connected to any channel"))?;
        let ch = channels.get_mut(&chan_id).expect("agent_channel/channels must agree");
        let sender = ch.sender.clone();
        let display_number = ch.display_number;
        let lost_sender = sender == agent;
        ch.members.retain(|m| m.agent_id != agent);
        let remaining = ch.members.clone();
        let closed = remaining.len() < 2 || lost_sender;
        if closed {
            for m in &remaining {
                agent_channel.remove(&m.agent_id);
            }
            channels.remove(&chan_id);
        }
        drop(agent_channel);
        drop(channels);

        self.audit(
            group,
            "human",
            "channel-disconnect",
            json!({ "channel_id": chan_id, "agent": agent, "remaining": remaining.len() }),
        );
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "orch-channel",
                // `sender` is the channel's sender BEFORE this removal —
                // still correct here: the still-open branch (`!closed`)
                // is only reached when `agent` was a receiver, i.e. the
                // sender is unchanged and still among `remaining`.
                channel_disconnected_event(
                    closed, &chan_id, agent, display_number,
                    self.channel_members_json(&sender, &remaining),
                ),
            );
        }
        if closed {
            let reason = if lost_sender {
                "the sender you were connected to disconnected — a channel needs a sender"
            } else {
                "the peer you were connected to disconnected"
            };
            for m in &remaining {
                if m.group != group {
                    self.audit(
                        &m.group,
                        "human",
                        "channel-disconnect",
                        json!({ "channel_id": chan_id, "agent": agent, "remaining": 0 }),
                    );
                }
                let _ = self.deliver_prompt(
                    &m.agent_id,
                    &format!("[orrerix] channel {chan_id} closed — {reason}."),
                    brand::AUDIT_ACTOR,
                    Delivery::MidSession,
                );
            }
        }
        Ok(json!({ "channel_id": chan_id, "closed": closed, "remaining": remaining.len() }))
    }

    /// Drop `agent_id` from its channel on death (`mark_dead` covers
    /// idle-kill, `kill_agent`, a crash, and planner auto-close identically)
    /// — the pane a channel message would land in is gone. Reuses
    /// `disconnect_agent`'s teardown-below-2 logic so a channel down to one
    /// live member closes exactly as it would from a human disconnect.
    pub(in crate::orchestration) fn cleanup_agent_channel(&self, agent_id: &str, group: &GroupId) {
        if self.agent_channel.lock_safe().contains_key(agent_id) {
            let _ = self.disconnect_agent(group, agent_id);
        }
    }

    /// Agent-facing (`channel_send` MCP tool, denied to planners —
    /// `require_not_planner` in mcp.rs): directional per #271 W3 addendum
    /// part B. The channel's **sender** may call this any time; it
    /// broadcasts to every OTHER member and grants each of them one reply
    /// credit (`may_reply = true`). Every other member (a **receiver**) may
    /// call this only when it currently holds that credit — delivering
    /// **solely to the sender** and consuming the credit — never to another
    /// receiver (B3/B4: request/response, star topology). The caller
    /// supplies ONLY text; the target(s) come exclusively from the
    /// membership graph plus the direction rule above (constraint 6). Errors
    /// if the caller isn't connected, or is a receiver with no credit.
    pub fn channel_send(&self, caller: &Caller, text: &str) -> Result<String, String> {
        let chan_id = self
            .agent_channel
            .lock_safe()
            .get(&caller.agent_id)
            .cloned()
            .ok_or("you are not connected to any channel — ask a human to connect this pane first")?;

        // Resolve targets AND, for a receiver, consume the reply credit —
        // all under one lock so a concurrent sender broadcast can't race the
        // credit check.
        let (targets, is_sender): (Vec<ChannelMember>, bool) = {
            let mut channels = self.channels.lock_safe();
            let ch = channels.get_mut(&chan_id).ok_or("channel no longer exists")?;
            if ch.sender == caller.agent_id {
                for m in ch.members.iter_mut().filter(|m| m.agent_id != caller.agent_id) {
                    m.may_reply = true;
                }
                let targets = ch.members.iter().filter(|m| m.agent_id != caller.agent_id).cloned().collect();
                (targets, true)
            } else {
                match ch.members.iter().find(|m| m.agent_id == caller.agent_id).map(|m| m.may_reply) {
                    None => return Err("channel no longer exists".into()),
                    Some(false) => {
                        return Err(
                            "you can only reply after the sender messages you — a receiver may \
                             never initiate, only answer (and only the sender, never another \
                             receiver)"
                                .into(),
                        )
                    }
                    Some(true) => {}
                }
                if let Some(m) = ch.members.iter_mut().find(|m| m.agent_id == caller.agent_id) {
                    m.may_reply = false; // consume the credit
                }
                let target = ch
                    .members
                    .iter()
                    .find(|m| m.agent_id == ch.sender)
                    .cloned()
                    .ok_or("channel no longer exists")?;
                (vec![target], false)
            }
        };

        let me = self.agent(&caller.agent_id).ok_or("unknown agent")?;
        let sender_label = self.channel_member_label(&ChannelMember {
            group: me.group.clone(),
            agent_id: me.id.clone(),
            name: me.name.clone(),
            role: me.role,
            may_reply: false,
        });
        let sanitized = notify::sanitize_gh_text(text, Self::CHANNEL_TEXT_CAP);
        let message = channel_message_text(&chan_id, &sender_label, &sanitized);

        for peer in &targets {
            self.audit(
                &peer.group,
                &caller.agent_id,
                "channel-message",
                json!({ "channel_id": chan_id, "from": caller.agent_id, "to": peer.agent_id, "text": sanitized }),
            );
            if peer.group != caller.group {
                self.audit(
                    &caller.group,
                    &caller.agent_id,
                    "channel-message",
                    json!({ "channel_id": chan_id, "from": caller.agent_id, "to": peer.agent_id, "text": sanitized }),
                );
            }
            let _ = self.deliver_prompt(&peer.agent_id, &message, brand::AUDIT_ACTOR, Delivery::MidSession);
        }
        Ok(if is_sender {
            format!("sent to {} peer(s) in {chan_id}", targets.len())
        } else {
            format!("replied to the sender in {chan_id}")
        })
    }

    /// Agent-facing (`channel_status` MCP tool, denied to planners): who the
    /// caller is connected to, the channel's sender, and — per peer — its
    /// `direction`/`can_send` (#271 W3 addendum A4: "a full peer sees
    /// exactly who can talk back"). Also reports the CALLER's own current
    /// `can_send` (true if it's the sender; if a receiver, true only while
    /// it holds the reply credit) — otherwise a receiver has no way to check
    /// before calling `channel_send` and hitting the credit error. Never
    /// mutates.
    pub fn channel_status(&self, caller: &Caller) -> Value {
        let chan_id = self.agent_channel.lock_safe().get(&caller.agent_id).cloned();
        let Some(chan_id) = chan_id else {
            return json!({ "connected": false, "channel_id": null, "peers": [] });
        };
        let ch = self.channels.lock_safe().get(&chan_id).cloned();
        let Some(ch) = ch else {
            return json!({ "connected": false, "channel_id": null, "peers": [] });
        };
        let my_can_send = ch.sender == caller.agent_id
            || ch.members.iter().any(|m| m.agent_id == caller.agent_id && m.may_reply);
        let peers: Vec<Value> = ch
            .members
            .iter()
            .filter(|m| m.agent_id != caller.agent_id)
            .map(|m| {
                let is_sender = m.agent_id == ch.sender;
                let has_token = self.agent_has_token(&m.agent_id);
                let can_send = is_sender || (m.may_reply && has_token);
                json!({
                    "agent_id": m.agent_id, "role": m.role, "name": m.name,
                    "repo": self.group(&m.group).map(|g| g.repo).unwrap_or_default(),
                    "direction": if is_sender { "sender" } else { "receiver" },
                    "can_send": can_send,
                    "delivery_only": !has_token,
                })
            })
            .collect();
        json!({
            "connected": true, "channel_id": chan_id, "sender": ch.sender,
            "display_number": ch.display_number, "can_send": my_can_send, "peers": peers,
        })
    }

    /// Every live channel — id, created time, sender, members — for the
    /// frontend's cross-tab indicators (tab switch, reconnect). Tauri-only
    /// (trusted webview); an agent only ever sees its own via `channel_status`.
    pub fn channel_list(&self) -> Value {
        let channels = self.channels.lock_safe();
        let mut list: Vec<&Channel> = channels.values().collect();
        list.sort_by_key(|c| c.created_ms);
        json!(list
            .iter()
            .map(|c| json!({
                "id": c.id,
                "created_ms": c.created_ms,
                "sender": c.sender,
                "display_number": c.display_number,
                "members": self.channel_members_json(&c.sender, &c.members),
            }))
            .collect::<Vec<_>>())
    }

    /// The channel `agent` (in `group`) belongs to, or `null`. Tauri-only,
    /// for a single pane's header chip. `group` is checked against the
    /// agent's actual group so a stale frontend reference reads as
    /// disconnected rather than leaking another pane's channel.
    pub fn channel_for_pane(&self, group: &GroupId, agent: &str) -> Value {
        match self.agent(agent) {
            Some(a) if a.group == group => {}
            _ => return Value::Null,
        }
        let Some(chan_id) = self.agent_channel.lock_safe().get(agent).cloned() else {
            return Value::Null;
        };
        let ch = self.channels.lock_safe().get(&chan_id).cloned();
        match ch {
            Some(c) => json!({
                "id": c.id, "sender": c.sender, "display_number": c.display_number,
                "members": self.channel_members_json(&c.sender, &c.members),
            }),
            None => Value::Null,
        }
    }

    /// Human-only (constraint 5): reassign a channel's sender without
    /// reconnecting (#271 W3 addendum, part B5). `new_sender_agent` must
    /// already be a member AND hold a token — a delivery-only member can
    /// never become sender, the same rule `connect_agents` enforces at
    /// mint/join time. Clears every member's reply credit (a swap
    /// invalidates in-flight "you may reply" state — the new sender starts
    /// clean) and notifies every member of its new role. Audited
    /// `channel-direction` in every distinct member group.
    pub fn set_sender(&self, channel_id: &str, new_sender_agent: &str) -> Result<Value, String> {
        let candidate = self.agent(new_sender_agent).ok_or("unknown agent: new_sender_agent")?;
        if candidate.token.is_empty() {
            return Err("a receive-only pane can't be the sender — it has no token".into());
        }
        let (from_sender, ch_clone) = {
            let mut channels = self.channels.lock_safe();
            let ch = channels.get_mut(channel_id).ok_or("unknown channel")?;
            if !ch.members.iter().any(|m| m.agent_id == new_sender_agent) {
                return Err("new sender must already be a member of this channel".into());
            }
            let from_sender = std::mem::replace(&mut ch.sender, new_sender_agent.to_string());
            for m in ch.members.iter_mut() {
                m.may_reply = false;
            }
            (from_sender, ch.clone())
        };

        let mut audited_groups = HashSet::new();
        for m in &ch_clone.members {
            if audited_groups.insert(m.group.clone()) {
                self.audit(
                    &m.group,
                    "human",
                    "channel-direction",
                    json!({ "channel_id": channel_id, "from_sender": from_sender, "to_sender": new_sender_agent }),
                );
            }
        }
        let member_json = self.channel_members_json(new_sender_agent, &ch_clone.members);
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "orch-channel",
                channel_updated_event(channel_id, new_sender_agent, ch_clone.display_number, member_json.clone()),
            );
        }
        for m in &ch_clone.members {
            let note = if m.agent_id == new_sender_agent {
                "[orrerix] you are now the SENDER of this channel — you may channel_send to \
                 everyone connected, any time."
            } else {
                "[orrerix] the sender of this channel changed — you are now a RECEIVER: \
                 channel_send works once the new sender messages you (reply-only)."
            };
            let _ = self.deliver_prompt(&m.agent_id, note, brand::AUDIT_ACTOR, Delivery::MidSession);
        }
        Ok(json!({
            "id": channel_id, "sender": new_sender_agent,
            "display_number": ch_clone.display_number, "members": member_json,
        }))
    }
}
