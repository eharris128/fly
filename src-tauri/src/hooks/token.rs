//! Per-pane authentication tokens for the hook channel (KTD7).
//!
//! Each pane gets a ≥128-bit CSPRNG token (here 256-bit). Tokens are compared
//! in constant time, and repeated invalid presentations lock the registry out
//! for a cooldown to blunt brute-force / spam.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngCore;
use subtle::ConstantTimeEq;

use crate::pty::PaneId;

/// 256-bit tokens — comfortably past the ≥128-bit floor (KTD7).
const TOKEN_BYTES: usize = 32;
/// Consecutive invalid presentations before a cooldown kicks in.
const MAX_FAILURES: u32 = 50;
/// How long the registry rejects everything after tripping the limit.
const LOCKOUT: Duration = Duration::from_secs(5);

struct Failures {
    count: u32,
    locked_until: Option<Instant>,
}

/// Maps live tokens to their panes. A `Vec` (not a `HashMap`) so validation
/// scans every entry, keeping timing independent of which token matched.
pub struct TokenRegistry {
    entries: Mutex<Vec<(String, PaneId)>>,
    failures: Mutex<Failures>,
}

impl Default for TokenRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            failures: Mutex::new(Failures {
                count: 0,
                locked_until: None,
            }),
        }
    }

    /// Issue and register a fresh token for `pane`. Must be called before the
    /// child starts so no callback can race registration (KTD7).
    pub fn issue(&self, pane: PaneId) -> String {
        let mut raw = [0u8; TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut raw);
        let token = to_hex(&raw);
        self.entries.lock().unwrap().push((token.clone(), pane));
        token
    }

    /// Revoke every token for a pane (on close / app exit).
    pub fn revoke(&self, pane: PaneId) {
        self.entries.lock().unwrap().retain(|(_, p)| *p != pane);
    }

    /// Resolve a presented token to its pane in constant time, or `None` if it
    /// is unknown or the registry is locked out.
    pub fn validate(&self, presented: &str) -> Option<PaneId> {
        {
            let mut f = self.failures.lock().unwrap();
            if let Some(until) = f.locked_until {
                if Instant::now() < until {
                    return None;
                }
                f.locked_until = None;
                f.count = 0;
            }
        }

        let presented = presented.as_bytes();
        let found = {
            let entries = self.entries.lock().unwrap();
            let mut found = None;
            for (tok, pane) in entries.iter() {
                // Constant-time per-token compare so a partial match can't be
                // discovered byte-by-byte. We scan all entries regardless.
                if tok.len() == presented.len()
                    && bool::from(tok.as_bytes().ct_eq(presented))
                {
                    found = Some(*pane);
                }
            }
            found
        };

        let mut f = self.failures.lock().unwrap();
        match found {
            Some(pane) => {
                f.count = 0;
                Some(pane)
            }
            None => {
                f.count += 1;
                if f.count >= MAX_FAILURES {
                    f.locked_until = Some(Instant::now() + LOCKOUT);
                }
                None
            }
        }
    }

    /// True while the registry is in a brute-force lockout (test/diagnostic).
    pub fn is_locked(&self) -> bool {
        let f = self.failures.lock().unwrap();
        f.locked_until.is_some_and(|until| Instant::now() < until)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
