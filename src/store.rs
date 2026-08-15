//! Persistent storage for the tea rota, backed by a JSON file on disk.
//!
//! One [`Rota`] is kept per Discord guild (server), keyed by guild id, so the
//! bot can serve several servers from a single file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A single person in the rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Discord user id, used to @mention the person.
    pub id: u64,
    /// A human-readable name, cached so listings read nicely.
    pub name: String,
}

impl Member {
    /// A Discord mention (`<@id>`) that pings the member.
    pub fn mention(&self) -> String {
        format!("<@{}>", self.id)
    }
}

/// A scheduled D&D session on a given date, with the person assigned to bring
/// tea (a snapshot of the rota at the time it was scheduled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Date, `YYYY/MM/DD`. Stored as a string so it round-trips through JSON
    /// cleanly and sorts chronologically as plain text.
    pub date: String,
    /// Who is bringing tea, snapshotted when the session was scheduled. `None`
    /// means the session is skipped — the group splits or doesn't use the rota,
    /// so nobody is on tea and no rota turn is consumed.
    pub assignee: Option<Member>,
    /// Optional free-text note (e.g. chapter, location).
    pub note: Option<String>,
}

impl Session {
    /// Whether this session skips the rota (no assignee).
    pub fn is_skipped(&self) -> bool {
        self.assignee.is_none()
    }
}

/// Why scheduling a session might fail.
#[derive(Debug, PartialEq, Eq)]
pub enum AddSessionError {
    /// The rota has no members, so there's nobody to assign.
    NoMembers,
    /// A session already exists on that date.
    DuplicateDate,
}

/// Why rescheduling a session might fail.
#[derive(Debug, PartialEq, Eq)]
pub enum RescheduleError {
    /// No session exists on the given `from` date.
    NotFound,
    /// A different session already exists on the target date.
    DuplicateDate,
}

/// Why reassigning a session's tea duty might fail.
#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    /// No session exists on the given date.
    SessionNotFound,
    /// The chosen substitute isn't in the rota.
    NotAMember,
}

/// The result of reassigning a session's tea duty.
#[derive(Debug, PartialEq, Eq)]
pub enum AssignOutcome {
    /// The session was moved from `old` (`None` if it was skipped) to `new`.
    Reassigned { old: Option<Member>, new: Member },
    /// The chosen person was already on tea for that session; nothing changed.
    Unchanged(Member),
}

/// A fixed rotation for one guild.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rota {
    /// The ordered list of people in the rotation.
    pub members: Vec<Member>,
    /// Index into `members` of whose turn it currently is.
    pub current: usize,
    /// Scheduled sessions, kept sorted by date ascending.
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Potsworth's one-and-only master for this server, set once via
    /// `/potsworth add`.
    #[serde(default)]
    master: Option<Member>,
    /// The channel `/potsworth add` was run in. Once set, Potsworth only
    /// operates here.
    #[serde(default)]
    master_channel: Option<u64>,
}

impl Rota {
    /// The member whose turn it currently is, if the rota is non-empty.
    pub fn current_member(&self) -> Option<&Member> {
        self.members.get(self.current)
    }

    /// Keep `current` pointing at a valid slot (wrapping / clamping as needed).
    fn normalise(&mut self) {
        if self.members.is_empty() {
            self.current = 0;
        } else {
            self.current %= self.members.len();
        }
    }

    /// Add a member to the end of the rotation. Returns `false` if they were
    /// already present.
    pub fn add(&mut self, member: Member) -> bool {
        if self.members.iter().any(|m| m.id == member.id) {
            return false;
        }
        self.members.push(member);
        self.normalise();
        true
    }

    /// Remove a member by id. Returns the removed member, if any.
    pub fn remove(&mut self, id: u64) -> Option<Member> {
        let pos = self.members.iter().position(|m| m.id == id)?;
        let removed = self.members.remove(pos);
        // Adjust the pointer so the "current" person doesn't silently change.
        if pos < self.current {
            self.current -= 1;
        }
        self.normalise();
        Some(removed)
    }

    /// Advance the rotation to the next person and return them.
    pub fn advance(&mut self) -> Option<&Member> {
        if self.members.is_empty() {
            return None;
        }
        self.current = (self.current + 1) % self.members.len();
        self.members.get(self.current)
    }

    /// Point the rotation at a specific member by id. Returns `false` if that
    /// member isn't in the rota.
    pub fn set_current(&mut self, id: u64) -> bool {
        match self.members.iter().position(|m| m.id == id) {
            Some(pos) => {
                self.current = pos;
                true
            }
            None => false,
        }
    }

    /// This server's master, if one has been assigned.
    pub fn master(&self) -> Option<&Member> {
        self.master.as_ref()
    }

    /// The channel Potsworth is bound to for this server, if any.
    pub fn master_channel(&self) -> Option<u64> {
        self.master_channel
    }

    /// Assign the master for this server, binding Potsworth to `channel_id`.
    /// This can only happen once: if a master is already set, the existing
    /// master is returned as an `Err` and nothing changes.
    pub fn set_master(&mut self, member: Member, channel_id: u64) -> Result<(), Member> {
        if let Some(existing) = &self.master {
            return Err(existing.clone());
        }
        self.master = Some(member);
        self.master_channel = Some(channel_id);
        Ok(())
    }

    /// Schedule a session on `date` (`YYYY/MM/DD`). If `skip` is false, assigns
    /// the current rota member and advances the rotation, returning the assigned
    /// member. If `skip` is true, the session has no assignee and the rota is
    /// left untouched (returns `None`) — used when the group splits or doesn't
    /// use the rota that session.
    pub fn add_session(
        &mut self,
        date: String,
        note: Option<String>,
        skip: bool,
    ) -> Result<Option<Member>, AddSessionError> {
        if self.sessions.iter().any(|s| s.date == date) {
            return Err(AddSessionError::DuplicateDate);
        }
        if skip {
            self.sessions.push(Session { date, assignee: None, note });
            self.sessions.sort_by(|a, b| a.date.cmp(&b.date));
            return Ok(None);
        }
        if self.members.is_empty() {
            return Err(AddSessionError::NoMembers);
        }
        let assignee = self.members[self.current].clone();
        self.sessions.push(Session {
            date,
            assignee: Some(assignee.clone()),
            note,
        });
        self.sessions.sort_by(|a, b| a.date.cmp(&b.date));
        self.advance();
        Ok(Some(assignee))
    }

    /// Mark the session on `date` as skipped (no assignee, splits / no rota).
    /// Returns `true` if a session was found. The rota order is not changed.
    pub fn skip_session(&mut self, date: &str) -> bool {
        match self.sessions.iter_mut().find(|s| s.date == date) {
            Some(s) => {
                s.assignee = None;
                true
            }
            None => false,
        }
    }

    /// Remove the session on `date`, returning it if it existed.
    pub fn remove_session(&mut self, date: &str) -> Option<Session> {
        let pos = self.sessions.iter().position(|s| s.date == date)?;
        Some(self.sessions.remove(pos))
    }

    /// Move the session on `from` to a new date `to`, keeping its assignee and
    /// note. Returns the updated session on success.
    pub fn reschedule_session(
        &mut self,
        from: &str,
        to: String,
    ) -> Result<Session, RescheduleError> {
        let pos = self
            .sessions
            .iter()
            .position(|s| s.date == from)
            .ok_or(RescheduleError::NotFound)?;
        // Allow moving a session "onto itself" (no-op), but not onto another.
        if self.sessions[pos].date != to && self.sessions.iter().any(|s| s.date == to) {
            return Err(RescheduleError::DuplicateDate);
        }
        self.sessions[pos].date = to.clone();
        self.sessions.sort_by(|a, b| a.date.cmp(&b.date));
        Ok(self.sessions.iter().find(|s| s.date == to).unwrap().clone())
    }

    /// Change who is on tea for the session on `date` to the rota member with
    /// `member_id`, then rebalance every session that hasn't happened yet.
    ///
    /// Fairness rule ("send substitute to back"): the substitute has just taken
    /// a turn, so they move to the end of the rotation. Every upcoming session
    /// (on or after `today`) *except* the one just covered is then reassigned by
    /// cycling through the new order from the front in date order, so tea duty
    /// is spread as evenly as possible across the remaining calendar. Sessions
    /// in the past are left untouched.
    pub fn assign_session(
        &mut self,
        date: &str,
        member_id: u64,
        today: &str,
    ) -> Result<AssignOutcome, AssignError> {
        let s_pos = self
            .sessions
            .iter()
            .position(|s| s.date == date)
            .ok_or(AssignError::SessionNotFound)?;
        let m_pos = self
            .members
            .iter()
            .position(|m| m.id == member_id)
            .ok_or(AssignError::NotAMember)?;

        // Already on tea for this session — nothing to do, and don't disturb
        // the rota order or the rest of the calendar.
        if self.sessions[s_pos].assignee.as_ref().is_some_and(|a| a.id == member_id) {
            return Ok(AssignOutcome::Unchanged(self.members[m_pos].clone()));
        }

        let old = self.sessions[s_pos].assignee.clone();
        let new = self.members[m_pos].clone();
        // Pin the covered session to the substitute.
        self.sessions[s_pos].assignee = Some(new.clone());

        // Move the substitute to the back of the rotation.
        let substitute = self.members.remove(m_pos);
        self.members.push(substitute);

        // Rebalance every other upcoming session, cycling through the new order
        // from the front in date order. Skipped sessions keep their turn free
        // and don't consume a slot. `current` continues the sequence so the
        // next newly-scheduled session stays fair too.
        let order = self.members.clone();
        let mut i = 0;
        for s in self.sessions.iter_mut() {
            if s.date.as_str() < today || s.date == date || s.is_skipped() {
                continue;
            }
            s.assignee = Some(order[i % order.len()].clone());
            i += 1;
        }
        self.current = i % order.len();

        Ok(AssignOutcome::Reassigned { old, new })
    }

    /// Sessions on or after `today` (ISO `YYYY-MM-DD`), in date order.
    pub fn upcoming<'a>(&'a self, today: &str) -> impl Iterator<Item = &'a Session> {
        let today = today.to_string();
        self.sessions.iter().filter(move |s| s.date >= today)
    }

    /// Sessions before `today` (ISO `YYYY-MM-DD`), in date order (oldest first).
    pub fn past<'a>(&'a self, today: &str) -> impl Iterator<Item = &'a Session> {
        let today = today.to_string();
        self.sessions.iter().filter(move |s| s.date < today)
    }
}

/// The whole persisted state: a rota per guild.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    /// Keyed by guild id (as a string so JSON keys are well-formed).
    rotas: HashMap<String, Rota>,
    #[serde(skip)]
    path: PathBuf,
}

impl Store {
    /// Load the store from `path`, or start empty if the file doesn't exist.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut store = match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
                eprintln!("Warning: could not parse {}: {e}. Starting fresh.", path.display());
                Store::default()
            }),
            Err(_) => Store::default(),
        };
        store.path = path;
        store
    }

    /// Get a mutable handle to a guild's rota, creating an empty one if needed.
    pub fn rota_mut(&mut self, guild_id: u64) -> &mut Rota {
        self.rotas.entry(guild_id.to_string()).or_default()
    }

    /// Read-only view of a guild's rota, if it exists (does not create one).
    pub fn rota(&self, guild_id: u64) -> Option<&Rota> {
        self.rotas.get(&guild_id.to_string())
    }

    /// Persist the current state to disk.
    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64) -> Member {
        Member { id, name: format!("p{id}") }
    }

    #[test]
    fn add_is_idempotent_by_id() {
        let mut r = Rota::default();
        assert!(r.add(m(1)));
        assert!(!r.add(m(1)));
        assert_eq!(r.members.len(), 1);
    }

    #[test]
    fn advance_wraps_around() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        r.add(m(3));
        assert_eq!(r.current_member().unwrap().id, 1);
        assert_eq!(r.advance().unwrap().id, 2);
        assert_eq!(r.advance().unwrap().id, 3);
        assert_eq!(r.advance().unwrap().id, 1); // wrapped
    }

    #[test]
    fn removing_before_current_keeps_same_person_up_next() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        r.add(m(3));
        r.set_current(3); // person 3 is up next
        assert_eq!(r.current_member().unwrap().id, 3);
        r.remove(1); // remove someone earlier in the list
        assert_eq!(r.current_member().unwrap().id, 3); // still person 3
    }

    #[test]
    fn removing_current_last_member_clamps() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        r.set_current(2); // person 2 up next, at the end
        r.remove(2);
        assert_eq!(r.current_member().unwrap().id, 1);
    }

    #[test]
    fn empty_rota_has_no_current() {
        let r = Rota::default();
        assert!(r.current_member().is_none());
    }

    #[test]
    fn master_can_only_be_set_once() {
        let mut r = Rota::default();
        assert!(r.master().is_none());
        assert!(r.master_channel().is_none());
        assert_eq!(r.set_master(m(1), 4242), Ok(()));
        assert_eq!(r.master().unwrap().id, 1);
        assert_eq!(r.master_channel(), Some(4242)); // bound to the channel
        // A second attempt is rejected and leaves master and channel unchanged.
        assert_eq!(r.set_master(m(2), 9999), Err(m(1)));
        assert_eq!(r.master().unwrap().id, 1);
        assert_eq!(r.master_channel(), Some(4242));
    }

    #[test]
    fn scheduling_sessions_cycles_through_the_rota() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        assert_eq!(r.add_session("2026-08-22".into(), None, false).unwrap().unwrap().id, 1);
        assert_eq!(r.add_session("2026-08-29".into(), None, false).unwrap().unwrap().id, 2);
        assert_eq!(r.add_session("2026-09-05".into(), None, false).unwrap().unwrap().id, 1); // wrapped
    }

    #[test]
    fn sessions_stay_sorted_by_date() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add_session("2026-09-05".into(), None, false).unwrap();
        r.add_session("2026-08-22".into(), None, false).unwrap();
        let dates: Vec<_> = r.sessions.iter().map(|s| s.date.as_str()).collect();
        assert_eq!(dates, ["2026-08-22", "2026-09-05"]);
    }

    #[test]
    fn cannot_schedule_without_members_or_on_a_duplicate_date() {
        let mut r = Rota::default();
        assert_eq!(
            r.add_session("2026-08-22".into(), None, false),
            Err(AddSessionError::NoMembers)
        );
        r.add(m(1));
        r.add_session("2026-08-22".into(), None, false).unwrap();
        assert_eq!(
            r.add_session("2026-08-22".into(), None, false),
            Err(AddSessionError::DuplicateDate)
        );
    }

    #[test]
    fn rescheduling_moves_the_date_and_keeps_it_sorted() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        r.add_session("2026-08-22".into(), Some("Chapter 5".into()), false).unwrap();
        r.add_session("2026-08-29".into(), None, false).unwrap();

        // Move the first session past the second.
        let moved = r
            .reschedule_session("2026-08-22", "2026-09-05".into())
            .unwrap();
        assert_eq!(moved.assignee.as_ref().unwrap().id, 1); // assignee unchanged
        assert_eq!(moved.note.as_deref(), Some("Chapter 5")); // note unchanged

        let dates: Vec<_> = r.sessions.iter().map(|s| s.date.as_str()).collect();
        assert_eq!(dates, ["2026-08-29", "2026-09-05"]); // re-sorted
    }

    #[test]
    fn rescheduling_reports_missing_and_clashing_dates() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add_session("2026-08-22".into(), None, false).unwrap();
        r.add_session("2026-08-29".into(), None, false).unwrap();

        assert_eq!(
            r.reschedule_session("2026-01-01", "2026-09-05".into()),
            Err(RescheduleError::NotFound)
        );
        assert_eq!(
            r.reschedule_session("2026-08-22", "2026-08-29".into()),
            Err(RescheduleError::DuplicateDate)
        );
    }

    /// Ids assigned to sessions, in stored (date) order (0 = skipped).
    fn assignee_ids(r: &Rota) -> Vec<u64> {
        r.sessions.iter().map(|s| s.assignee.as_ref().map_or(0, |a| a.id)).collect()
    }

    #[test]
    fn reassigning_rebalances_all_upcoming_sessions() {
        let mut r = Rota::default();
        r.add(m(1)); // Alice
        r.add(m(2)); // Bob
        r.add(m(3)); // Carol
        // Auto-assigned in rotation order.
        r.add_session("2026-08-22".into(), None, false).unwrap(); // Alice
        r.add_session("2026-08-29".into(), None, false).unwrap(); // Bob
        r.add_session("2026-09-05".into(), None, false).unwrap(); // Carol
        r.add_session("2026-09-12".into(), None, false).unwrap(); // Alice

        // Bob covers the 22nd (Alice can't make it).
        let outcome = r.assign_session("2026-08-22", 2, "2026-08-01").unwrap();
        assert_eq!(outcome, AssignOutcome::Reassigned { old: Some(m(1)), new: m(2) });

        // Bob is sent to the back: order becomes [Alice, Carol, Bob].
        assert_eq!(r.members.iter().map(|m| m.id).collect::<Vec<_>>(), [1, 3, 2]);
        // 22nd is pinned to Bob; the rest cycle Alice, Carol, Bob from the front.
        assert_eq!(assignee_ids(&r), [2, 1, 3, 2]);
        // The next newly-scheduled session continues the sequence (back to Alice).
        assert_eq!(r.current_member().unwrap().id, 1);
    }

    #[test]
    fn reassigning_leaves_past_sessions_untouched() {
        let mut r = Rota::default();
        r.add(m(1)); // Alice
        r.add(m(2)); // Bob
        r.add_session("2026-08-01".into(), None, false).unwrap(); // Alice (past)
        r.add_session("2026-08-22".into(), None, false).unwrap(); // Bob
        r.add_session("2026-08-29".into(), None, false).unwrap(); // Alice

        // Today is the 15th, so the 1st has already happened.
        r.assign_session("2026-08-22", 1, "2026-08-15").unwrap(); // Alice covers the 22nd

        // Past session keeps its original assignee (Alice); Alice sent to back
        // gives order [Bob, Alice]; the only other upcoming session (29th) is
        // reassigned to Bob (front).
        assert_eq!(assignee_ids(&r), [1, 1, 2]);
    }

    #[test]
    fn reassigning_to_the_same_person_is_a_no_op() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        r.add_session("2026-08-22".into(), None, false).unwrap(); // assigned to Alice
        let order_before: Vec<_> = r.members.iter().map(|m| m.id).collect();
        let current_before = r.current;

        assert_eq!(
            r.assign_session("2026-08-22", 1, "2026-08-01").unwrap(),
            AssignOutcome::Unchanged(m(1))
        );
        // Order, pointer and assignees untouched.
        assert_eq!(r.members.iter().map(|m| m.id).collect::<Vec<_>>(), order_before);
        assert_eq!(r.current, current_before);
        assert_eq!(assignee_ids(&r), [1]);
    }

    #[test]
    fn reassigning_reports_missing_session_and_non_members() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add_session("2026-08-22".into(), None, false).unwrap();
        assert_eq!(
            r.assign_session("2099-01-01", 1, "2026-08-01"),
            Err(AssignError::SessionNotFound)
        );
        assert_eq!(
            r.assign_session("2026-08-22", 999, "2026-08-01"),
            Err(AssignError::NotAMember)
        );
    }

    #[test]
    fn upcoming_filters_out_past_dates() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add_session("2026-08-01".into(), None, false).unwrap();
        r.add_session("2026-08-22".into(), None, false).unwrap();
        r.add_session("2026-09-05".into(), None, false).unwrap();
        let upcoming: Vec<_> = r.upcoming("2026-08-15").map(|s| s.date.as_str()).collect();
        assert_eq!(upcoming, ["2026-08-22", "2026-09-05"]);
    }

    #[test]
    fn skipped_session_has_no_assignee_and_does_not_advance_the_rota() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add(m(2));
        // A skipped session takes no assignee and leaves the pointer alone.
        assert_eq!(r.add_session("2026-08-22".into(), None, true).unwrap(), None);
        assert!(r.sessions[0].is_skipped());
        assert_eq!(r.current, 0);
        // The next normal session still goes to the first person.
        assert_eq!(
            r.add_session("2026-08-29".into(), None, false).unwrap().unwrap().id,
            1
        );
    }

    #[test]
    fn can_skip_with_an_empty_rota() {
        let mut r = Rota::default();
        assert_eq!(r.add_session("2026-08-22".into(), None, true).unwrap(), None);
        assert!(r.sessions[0].is_skipped());
    }

    #[test]
    fn skip_session_marks_an_existing_session_and_rebalance_ignores_it() {
        let mut r = Rota::default();
        r.add(m(1)); // Alice
        r.add(m(2)); // Bob
        r.add_session("2026-08-22".into(), None, false).unwrap(); // Alice
        r.add_session("2026-08-29".into(), None, false).unwrap(); // Bob
        r.add_session("2026-09-05".into(), None, false).unwrap(); // Alice

        // Split the middle session.
        assert!(r.skip_session("2026-08-29"));
        assert_eq!(assignee_ids(&r), [1, 0, 1]); // 0 = skipped

        // Bob covers the first; rebalance must leave the skipped one alone and
        // not consume a rotation slot for it.
        r.assign_session("2026-08-22", 2, "2026-08-01").unwrap();
        // order becomes [Alice, Bob]; 22nd pinned to Bob(2), 29th stays skipped,
        // 5 Sep gets the front of the cycle → Alice(1).
        assert_eq!(assignee_ids(&r), [2, 0, 1]);
    }

    #[test]
    fn past_returns_only_dates_before_today() {
        let mut r = Rota::default();
        r.add(m(1));
        r.add_session("2026-08-01".into(), None, false).unwrap();
        r.add_session("2026-08-22".into(), None, false).unwrap();
        r.add_session("2026-09-05".into(), None, false).unwrap();
        let past: Vec<_> = r.past("2026-08-15").map(|s| s.date.as_str()).collect();
        assert_eq!(past, ["2026-08-01"]);
    }
}
