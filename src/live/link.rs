//! Command-line / URL arguments: `sdat://join/<ticket>`, `tidal://track/<id>`
//! or an in-app route.

use tonearm_core::live::ticket::{SCHEME_PREFIX, Ticket, TicketError};

/// What a URL passed on the command line (or by the desktop's scheme
/// handler) asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// A Listen Together invitation; holds the normalized `sdat://join/` URL.
    Join(String),
    /// An `sdat://` URL that is not a usable invitation.
    BadInvitation(TicketError),
    /// `tidal://track/<id>`.
    TidalTrack(String),
    /// Anything else is an in-app route.
    Route(String),
}

fn has_prefix(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Classifies one URL argument.
pub fn classify(arg: &str) -> Arg {
    let s = arg.trim();
    if has_prefix(s, "sdat://") {
        return match parse_invitation(s) {
            Ok(url) => Arg::Join(url),
            Err(e) => Arg::BadInvitation(e),
        };
    }
    if let Some(id) = s.strip_prefix("tidal://track/") {
        return Arg::TidalTrack(id.to_string());
    }
    Arg::Route(s.to_string())
}

/// Validates pasted text or a URL and returns the canonical
/// `sdat://join/<ticket>` link. Invitations are always links; a bare ticket
/// is accepted as pasted text too.
pub fn parse_invitation(text: &str) -> Result<String, TicketError> {
    let s = text.trim();
    if has_prefix(s, "sdat://") && !has_prefix(s, SCHEME_PREFIX) {
        return Err(TicketError::NotInvitation);
    }
    Ticket::parse(s).map(|t| t.to_url())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> Ticket {
        Ticket { v: tonearm_core::live::PROTO_VERSION, addr: "{\"id\":\"x\"}".into(), secret: "s3cret".into(), name: Some("Ana".into()) }
    }

    #[test]
    fn join_link_is_recognised() {
        let url = ticket().to_url();
        assert_eq!(classify(&url), Arg::Join(url.clone()));
        assert_eq!(classify(&format!("  {url}\n")), Arg::Join(url));
    }

    #[test]
    fn scheme_is_case_insensitive() {
        let url = ticket().to_url();
        let upper = url.replacen("sdat://join/", "SDAT://JOIN/", 1);
        assert_eq!(classify(&upper), Arg::Join(url));
    }

    #[test]
    fn trailing_slash_is_normalised() {
        let url = ticket().to_url();
        assert_eq!(parse_invitation(&format!("{url}/")), Ok(url));
    }

    #[test]
    fn bare_ticket_pasted() {
        let url = ticket().to_url();
        let bare = url.trim_start_matches(SCHEME_PREFIX);
        assert_eq!(parse_invitation(bare), Ok(url));
    }

    #[test]
    fn other_sdat_urls_are_not_invitations() {
        assert_eq!(classify("sdat://settings"), Arg::BadInvitation(TicketError::NotInvitation));
        assert_eq!(parse_invitation("https://example.com/join/abc"), Err(TicketError::NotInvitation));
    }

    #[test]
    fn damaged_invitation() {
        assert_eq!(classify("sdat://join/"), Arg::BadInvitation(TicketError::Malformed));
        assert_eq!(classify("sdat://join/!!!"), Arg::BadInvitation(TicketError::Malformed));
    }

    #[test]
    fn newer_protocol_is_rejected() {
        let mut t = ticket();
        t.v = tonearm_core::live::PROTO_VERSION + 1;
        assert_eq!(classify(&t.to_url()), Arg::BadInvitation(TicketError::TooNew));
    }

    #[test]
    fn existing_arguments_keep_working() {
        assert_eq!(classify("tidal://track/12345"), Arg::TidalTrack("12345".into()));
        assert_eq!(classify("album/42"), Arg::Route("album/42".into()));
    }
}
