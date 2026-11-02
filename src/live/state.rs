//! UI state of a Listen Together session and how transport events move it.
//! Pure (no GTK, no network) so it can be unit tested.

use tonearm_core::live::net::LiveEvent;
use tonearm_core::live::proto::{EndReason, RejectReason};
use tonearm_core::live::session::GuestEvent;

use crate::i18n::{tr, trf};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Phase {
    #[default]
    Idle,
    /// Host: opening the endpoint.
    Starting,
    Hosting { url: String, guests: Vec<String> },
    /// Guest: dialing the host.
    Joining,
    Listening { host: String },
    Reconnecting { host: String },
    /// The session is over; the panel shows why until dismissed.
    Ended(Ended),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// The host closed it.
    Closed,
    /// The host went silent.
    Lost,
    Rejected(RejectReason),
    /// Could not start or join (the error itself goes to the log).
    Failed,
}

impl Phase {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Idle | Self::Ended(_))
    }
    pub fn is_guest(&self) -> bool {
        matches!(self, Self::Joining | Self::Listening { .. } | Self::Reconnecting { .. })
    }
    pub fn is_host(&self) -> bool {
        matches!(self, Self::Starting | Self::Hosting { .. })
    }
}

/// The phase after `ev`, or `None` when it does not change.
/// Events arriving after the session ended locally are ignored.
pub fn apply_event(phase: &Phase, ev: &LiveEvent) -> Option<Phase> {
    match (phase, ev) {
        (Phase::Hosting { url, guests }, LiveEvent::Guests(list)) => {
            let names: Vec<String> = list.iter().map(|g| g.name.clone()).collect();
            (names != *guests).then(|| Phase::Hosting { url: url.clone(), guests: names })
        }
        (p, LiveEvent::Guest(GuestEvent::Joined { host_name })) if p.is_guest() => Some(Phase::Listening { host: host_name.clone() }),
        (p, LiveEvent::Guest(GuestEvent::Rejected(r))) if p.is_guest() => Some(Phase::Ended(Ended::Rejected(*r))),
        (p, LiveEvent::Guest(GuestEvent::Ended(reason))) if p.is_guest() => Some(match reason {
            EndReason::HostEnded => Phase::Ended(Ended::Closed),
            EndReason::Left => Phase::Idle,
            EndReason::HostLost | EndReason::Other => Phase::Ended(Ended::Lost),
        }),
        (Phase::Listening { host }, LiveEvent::Reconnecting) => Some(Phase::Reconnecting { host: host.clone() }),
        _ => None,
    }
}

/// Message shown when a session is over.
pub fn ended_message(e: &Ended) -> String {
    match e {
        Ended::Closed => tr("The host ended the session"),
        Ended::Lost => tr("Lost connection to the host"),
        Ended::Rejected(RejectReason::Full) => tr("This session is full"),
        Ended::Rejected(RejectReason::IncompatibleVersion) => tr("The host uses another version of the app. Update both apps and try again."),
        Ended::Rejected(RejectReason::BadSecret | RejectReason::Other) => tr("This invitation is no longer valid"),
        Ended::Failed => tr("Could not connect. Check your network and try again."),
    }
}

/// One-line status for the panel header.
pub fn status_text(p: &Phase) -> String {
    match p {
        Phase::Idle => tr("Listen to the same music in sync, each with your own TIDAL account"),
        Phase::Starting => tr("Starting session…"),
        Phase::Hosting { guests, .. } if guests.is_empty() => tr("Waiting for listeners"),
        Phase::Hosting { guests, .. } => crate::i18n::trn("%d listener", "%d listeners", i64::try_from(guests.len()).unwrap_or(i64::MAX), &[&guests.len()]),
        Phase::Joining => tr("Joining…"),
        Phase::Listening { host } => trf("Listening with %s", &[host]),
        Phase::Reconnecting { .. } => tr("Reconnecting…"),
        Phase::Ended(_) => tr("Session ended"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonearm_core::live::session::GuestInfo;

    fn hosting(guests: &[&str]) -> Phase {
        Phase::Hosting { url: "sdat://join/x".into(), guests: guests.iter().map(ToString::to_string).collect() }
    }

    #[test]
    fn host_guest_list_follows_events() {
        let ev = LiveEvent::Guests(vec![GuestInfo { peer: "p1".into(), name: "Ana".into() }]);
        assert_eq!(apply_event(&hosting(&[]), &ev), Some(hosting(&["Ana"])));
        assert_eq!(apply_event(&hosting(&["Ana"]), &ev), None);
        assert_eq!(apply_event(&hosting(&["Ana"]), &LiveEvent::Guests(vec![])), Some(hosting(&[])));
    }

    #[test]
    fn guest_lifecycle() {
        let joined = LiveEvent::Guest(GuestEvent::Joined { host_name: "Ana".into() });
        let listening = Phase::Listening { host: "Ana".into() };
        assert_eq!(apply_event(&Phase::Joining, &joined), Some(listening.clone()));
        assert_eq!(apply_event(&listening, &LiveEvent::Reconnecting), Some(Phase::Reconnecting { host: "Ana".into() }));
        assert_eq!(apply_event(&Phase::Reconnecting { host: "Ana".into() }, &joined), Some(listening.clone()));
        let end = LiveEvent::Guest(GuestEvent::Ended(EndReason::HostEnded));
        assert_eq!(apply_event(&listening, &end), Some(Phase::Ended(Ended::Closed)));
        let lost = LiveEvent::Guest(GuestEvent::Ended(EndReason::HostLost));
        assert_eq!(apply_event(&Phase::Joining, &lost), Some(Phase::Ended(Ended::Lost)));
    }

    #[test]
    fn rejection_ends_the_session() {
        let ev = LiveEvent::Guest(GuestEvent::Rejected(RejectReason::Full));
        assert_eq!(apply_event(&Phase::Joining, &ev), Some(Phase::Ended(Ended::Rejected(RejectReason::Full))));
    }

    #[test]
    fn leaving_returns_to_idle() {
        let ev = LiveEvent::Guest(GuestEvent::Ended(EndReason::Left));
        assert_eq!(apply_event(&Phase::Listening { host: "Ana".into() }, &ev), Some(Phase::Idle));
    }

    #[test]
    fn late_events_are_ignored() {
        let end = LiveEvent::Guest(GuestEvent::Ended(EndReason::HostEnded));
        assert_eq!(apply_event(&Phase::Idle, &end), None);
        assert_eq!(apply_event(&Phase::Ended(Ended::Lost), &end), None);
        assert_eq!(apply_event(&Phase::Idle, &LiveEvent::Guests(vec![])), None);
        assert_eq!(apply_event(&hosting(&[]), &end), None);
        assert_eq!(apply_event(&Phase::Joining, &LiveEvent::Reconnecting), None);
    }

    #[test]
    fn roles() {
        assert!(hosting(&[]).is_host() && hosting(&[]).is_active());
        assert!(Phase::Joining.is_guest());
        assert!(!Phase::Ended(Ended::Failed).is_active());
        assert!(!Phase::Idle.is_guest() && !Phase::Idle.is_host());
    }

    #[test]
    fn messages_are_not_empty() {
        for e in [Ended::Closed, Ended::Lost, Ended::Failed, Ended::Rejected(RejectReason::Full), Ended::Rejected(RejectReason::BadSecret), Ended::Rejected(RejectReason::IncompatibleVersion)] {
            assert!(!ended_message(&e).is_empty());
        }
        assert_eq!(status_text(&Phase::Listening { host: "Ana".into() }), "Listening with Ana");
    }
}
