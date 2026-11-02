//! Invitations: `sdat://join/<ticket>`, meant to be shown as a QR code.
//!
//! The ticket carries everything needed to dial the host (its iroh endpoint
//! address, serialized by the transport) plus a random session secret; no
//! server of ours is involved.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

pub const SCHEME_PREFIX: &str = "sdat://join/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// Protocol version of the host.
    pub v: u32,
    /// Transport address of the host (opaque to this module).
    pub addr: String,
    /// Session secret; guests must present it in `Hello`.
    pub secret: String,
    /// Display name of the host (optional, shown before joining).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    #[error("not an SDAT invitation")]
    NotInvitation,
    #[error("damaged invitation")]
    Malformed,
    #[error("invitation from a newer SDAT, update the app")]
    TooNew,
}

impl Ticket {
    pub fn to_url(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        format!("{SCHEME_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Accepts the URL or the bare ticket (pasted text, QR content).
    pub fn parse(s: &str) -> Result<Self, TicketError> {
        let s = s.trim();
        let n = SCHEME_PREFIX.len();
        let body = if s.len() >= n && s.is_char_boundary(n) && s[..n].eq_ignore_ascii_case(SCHEME_PREFIX) {
            &s[n..]
        } else if s.contains("://") {
            return Err(TicketError::NotInvitation);
        } else {
            s
        };
        let body = body.trim_end_matches('/');
        if body.is_empty() || body.len() > 4096 {
            return Err(TicketError::Malformed);
        }
        let bytes = URL_SAFE_NO_PAD.decode(body).map_err(|_e| TicketError::Malformed)?;
        let t: Ticket = serde_json::from_slice(&bytes).map_err(|_e| TicketError::Malformed)?;
        if t.v > super::PROTO_VERSION {
            return Err(TicketError::TooNew);
        }
        if t.addr.is_empty() || t.secret.is_empty() {
            return Err(TicketError::Malformed);
        }
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Ticket {
        Ticket { v: 1, addr: "endpoint-xyz".into(), secret: "s3cr3t".into(), name: Some("Nico".into()) }
    }

    #[test]
    fn scheme_is_case_insensitive() {
        let u = t().to_url().replacen("sdat://join/", "SDAT://Join/", 1);
        assert_eq!(Ticket::parse(&u), Ok(t()));
    }

    #[test]
    fn round_trip() {
        let u = t().to_url();
        assert!(u.starts_with("sdat://join/"));
        assert_eq!(Ticket::parse(&u), Ok(t()));
        assert_eq!(Ticket::parse(&format!("  {u}/ ")), Ok(t()));
        assert_eq!(Ticket::parse(u.strip_prefix(SCHEME_PREFIX).unwrap()), Ok(t()));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Ticket::parse("https://example.com"), Err(TicketError::NotInvitation));
        assert_eq!(Ticket::parse("sdat://join/"), Err(TicketError::Malformed));
        assert_eq!(Ticket::parse("sdat://join/!!!"), Err(TicketError::Malformed));
    }

    #[test]
    fn newer_version_says_update() {
        let u = Ticket { v: 99, ..t() }.to_url();
        assert_eq!(Ticket::parse(&u), Err(TicketError::TooNew));
    }
}
