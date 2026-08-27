//! Phone screenshot drop — storage, sniffing, and prompt composition
//! (phone-screenshot-drop U1–U3).
//!
//! This module owns everything between "bytes arrive on the wire" and "text is
//! ready for the pane": where the image lands (this file's path helpers),
//! what format it actually is (`sniff_image`, U2), how it is written durably
//! (`DropStore`, U2), and the exact text the agent sees (`compose_drop_prompt`,
//! U3). The route that drives it lives in `server.rs` (U6).
//!
//! Everything here is pure or filesystem-only — no shell, no PTY, no HTTP — so
//! it is unit-testable without a running app.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::automations::store::{create_private_dir, sync_parent_dir};

/// Bytes of the body buffered before any file is created — enough for every
/// pattern in [`sniff_image`] (the longest, an ISO-BMFF `ftyp` major brand,
/// ends at byte 12).
const SNIFF_BYTES: usize = 16;

/// Copy buffer for the streaming body write. Fixed size is the point: fly's own
/// memory stays O(1) regardless of image size (KTD1).
const COPY_CHUNK: usize = 64 * 1024;

/// Prefix marking a temp file as fly's, so the startup sweep can distinguish
/// crash residue from a user's own files — the drop directory is somewhere the
/// user browses (KTD4), so the sweep must never touch anything else.
const TEMP_PREFIX: &str = ".fly-drop-tmp-";

/// Age past which a prefixed temp file is presumed crash residue and swept
/// (KTD7). Comfortably longer than any real upload.
const TEMP_STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Why a configured `feed.dropDir` could not be turned into a usable absolute
/// path. Every variant is a *refusal*, never a silent fallback: resolving a bad
/// shape against the process cwd would put screenshots in `/` for a GUI launched
/// from a desktop file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropDirError {
    /// The configured value was empty or entirely whitespace.
    Empty,
    /// Neither absolute nor tilde-prefixed — e.g. `inbox` or `./shots`.
    Relative,
    /// `~user/...`. Expanding this needs a passwd lookup fly does not do.
    ForeignHome,
    /// Tilde-prefixed but `$HOME` could not be determined.
    NoHome,
}

impl fmt::Display for DropDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "feed.dropDir is empty",
            Self::Relative => "feed.dropDir must be absolute or start with `~/`",
            Self::ForeignHome => "feed.dropDir uses `~user/`, which fly does not expand",
            Self::NoHome => "feed.dropDir starts with `~` but $HOME is unset",
        };
        f.write_str(msg)
    }
}

/// Expand a leading `~` against `home` and require the result be absolute.
///
/// Pure — `home` is injected rather than read from the environment, so the
/// tests below pin the expansion without touching `$HOME`.
///
/// This is deliberately **not** applied at deserialization. `set_config`
/// round-trips the whole `Config` back to disk, so an expansion baked into the
/// in-memory struct would be persisted the first time the settings menu saves
/// anything, silently rewriting the user's `~/projects/inbox` and freezing
/// `$HOME` into their config file. See [`crate::config::FeedConfig::drop_dir`].
///
/// Absoluteness is enforced here rather than in a second validator so there is
/// exactly one place that decides what a legal `dropDir` looks like.
pub fn expand_tilde(raw: &str, home: Option<&Path>) -> Result<PathBuf, DropDirError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DropDirError::Empty);
    }
    let Some(rest) = raw.strip_prefix('~') else {
        // No tilde: the only other legal shape is an absolute path.
        return if Path::new(raw).is_absolute() {
            Ok(PathBuf::from(raw))
        } else {
            Err(DropDirError::Relative)
        };
    };
    // `~`, `~/…`, or `~user/…` — only the first two are ours to expand.
    let tail = match rest {
        "" => None,
        r if r.starts_with('/') => Some(r.trim_start_matches('/')),
        _ => return Err(DropDirError::ForeignHome),
    };
    let home = home.ok_or(DropDirError::NoHome)?;
    Ok(match tail {
        // `~/` with nothing after it is still just `$HOME`.
        Some(t) if !t.is_empty() => home.join(t),
        _ => home.to_path_buf(),
    })
}

/// Resolve the effective drop directory: the configured value if set, else
/// `<data root>/inbox` (KTD4).
///
/// The unconfigured default goes under the data root — and so *does* stay
/// per-flavor isolated — while an explicitly configured directory is taken at
/// face value. That asymmetry is intended: the default has to put files
/// somewhere defensible, but a user who names a directory means that directory.
pub fn resolve_drop_dir(
    configured: Option<&str>,
    home: Option<&Path>,
    data_root: &Path,
) -> Result<PathBuf, DropDirError> {
    match configured {
        Some(raw) => expand_tilde(raw, home),
        None => Ok(data_root.join("inbox")),
    }
}

/// An image format fly is willing to store, as determined by the *bytes* —
/// never by the client-declared content type, which is attacker-controlled and
/// on iOS sometimes simply wrong (R16, KTD8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    /// HEIC/HEIF. Stored, not rejected: iOS *screenshots* are PNG so the common
    /// path is unaffected, but a camera photo may arrive as HEIC. Refusing a
    /// file fly can perfectly well store, on a guess about what the agent's
    /// image reader supports, is the wrong call — the page warns instead.
    Heic,
    Avif,
}

impl ImageKind {
    /// The stored extension. An exhaustive match over a fixed set of literals,
    /// so no client-supplied string can ever reach the path (R16).
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Heic => "heic",
            Self::Avif => "avif",
        }
    }
}

/// ISO-BMFF `ftyp` major brands that mean HEIC/HEIF.
const HEIC_BRANDS: [&[u8; 4]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// ISO-BMFF `ftyp` major brands that mean AVIF.
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];

/// Identify an image from its leading bytes, or abstain.
///
/// Total and abstain-on-surprise: an unrecognized prefix returns `None` rather
/// than guessing, matching the posture of every other detector in this
/// codebase. PNG/JPEG/GIF/WebP follow the WHATWG MIME-sniffing image patterns;
/// HEIC and AVIF are not in that spec and are matched by their ISO-BMFF `ftyp`
/// major brand, which is where those formats actually declare themselves.
pub fn sniff_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageKind::Png);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(ImageKind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    // RIFF is a container family — the WebP marker is at bytes 8..12, so a WAV
    // or AVI must not match on the `RIFF` prefix alone.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageKind::Webp);
    }
    // ISO-BMFF: a leading box whose type is `ftyp`, with the major brand next.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand: &[u8] = &bytes[8..12];
        if HEIC_BRANDS.iter().any(|b| b.as_slice() == brand) {
            return Some(ImageKind::Heic);
        }
        if AVIF_BRANDS.iter().any(|b| b.as_slice() == brand) {
            return Some(ImageKind::Avif);
        }
    }
    None
}

/// Mint the stored filename: `<UTC timestamp>-<16 hex chars>.<ext>`.
///
/// Both inputs are injected so the function is deterministic under test. **No
/// client-supplied component participates** (R16) — not the declared filename,
/// not the content type, not the caption. The extension comes from the sniffed
/// [`ImageKind`], so the whole name is drawn from a fixed alphabet with no path
/// separator and no `..` possible.
///
/// The random half also makes names globally unique across the stable and dev
/// flavors, which is what lets them share one directory (KTD4).
pub fn mint_filename(now: DateTime<Utc>, rand: [u8; 8], kind: ImageKind) -> String {
    let mut hex = String::with_capacity(16);
    for b in rand {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    format!(
        "{}-{}.{}",
        now.format("%Y%m%dT%H%M%SZ"),
        hex,
        kind.extension()
    )
}

/// Why a store attempt failed. The route maps each to a distinct wire code, so
/// the phone can tell "too big" from "not an image" from "the disk is broken"
/// (R5, R12, R16, R17).
#[derive(Debug)]
pub enum DropError {
    /// The body exceeded the cap. Nothing is retained.
    Oversize,
    /// The leading bytes matched no known image format.
    BadFormat,
    /// The directory is unwritable, the disk is full, or a rename failed.
    Storage(io::Error),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize => f.write_str("image exceeds the configured size cap"),
            Self::BadFormat => f.write_str("body is not a recognized image format"),
            Self::Storage(e) => write!(f, "drop storage failed: {e}"),
        }
    }
}

impl From<io::Error> for DropError {
    fn from(e: io::Error) -> Self {
        Self::Storage(e)
    }
}

/// The destination directory for phone drops, with the durable-write discipline
/// the rest of fly's stores use: directory `0700`, file `0600`, temp-in-the-same
/// directory then rename, `sync_all` before the rename.
///
/// **Note what is deliberately *not* reused.**
/// [`crate::automations::store::write_atomic_owner_only`] takes a fully
/// materialized `&[u8]`; calling it with an image body would reintroduce the
/// 25 MiB buffer KTD1 exists to avoid. This type writes its own streaming temp
/// file while reusing that module's `create_private_dir` and `sync_parent_dir`
/// directly. The atomic writer is the *discipline* precedent here, not the
/// primitive.
pub struct DropStore {
    dir: PathBuf,
}

impl DropStore {
    /// Create the directory if absent, canonicalize it once, and sweep stale
    /// crash residue.
    ///
    /// A failure here is **retained and reported per request**, never a reason
    /// to refuse to start the feed listener — AE8 depends on an unwritable drop
    /// directory surfacing as a `storageFailed` response rather than taking the
    /// whole feed (and the dashboard that reads it) down with it. See the
    /// construction site in `lib.rs`.
    pub fn new(dir: &Path) -> io::Result<Self> {
        create_private_dir(dir)?;
        // Canonicalize once, so every later `starts_with` scope check compares
        // resolved paths (the `read_bundle_scoped` precedent).
        let dir = fs::canonicalize(dir)?;
        sweep_stale_temps(&dir);
        Ok(Self { dir })
    }

    /// The canonicalized destination directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stream `reader` into a temp file in this directory, refusing anything
    /// over `cap` bytes or whose leading bytes are not an image.
    ///
    /// Ordering is load-bearing (KTD7): the first [`SNIFF_BYTES`] are buffered
    /// and matched *before* any file is created, so a non-image body never
    /// touches the filesystem at all. Past that point the returned
    /// [`StoredImage`] owns the temp file and unlinks it on drop, so an early
    /// return anywhere in the caller cannot orphan one.
    pub fn store(&self, reader: &mut impl Read, cap: u64) -> Result<StoredImage, DropError> {
        let mut head = [0u8; SNIFF_BYTES];
        let head_len = read_up_to(reader, &mut head)?;
        let head = &head[..head_len];

        // Refuse before creating anything. A body shorter than the shortest
        // pattern simply fails to match, which is the same abstention.
        let kind = sniff_image(head).ok_or(DropError::BadFormat)?;
        if head_len as u64 > cap {
            return Err(DropError::Oversize);
        }

        let (temp_path, mut file) = self.create_temp()?;
        // From here on `guard` owns the unlink, so every `?` below cleans up.
        let guard = StoredImage {
            temp: Some(temp_path),
            dest: self.dir.join(mint_filename(Utc::now(), random_bytes(), kind)),
            kind,
        };

        file.write_all(head)?;
        let mut written = head_len as u64;

        // `take(remaining + 1)` bounds the read at one byte past the cap: if
        // that extra byte materializes the body is oversize, and we know it
        // without having read (or written) anything more.
        let remaining = cap - written;
        let mut bounded = reader.take(remaining.saturating_add(1));
        let mut buf = vec![0u8; COPY_CHUNK];
        loop {
            let n = bounded.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written = written.saturating_add(n as u64);
            if written > cap {
                // `guard` unlinks the partial temp file as it drops.
                return Err(DropError::Oversize);
            }
            file.write_all(&buf[..n])?;
        }

        // Durability before delivery is not optional here: the agent is about to
        // be asked to read this path, immediately.
        file.sync_all()?;
        Ok(guard)
    }

    /// Create a uniquely-named `0600` temp file in the destination directory
    /// (same filesystem, so the later rename is atomic).
    fn create_temp(&self) -> io::Result<(PathBuf, fs::File)> {
        let mut hex = String::with_capacity(16);
        for b in random_bytes() {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        let path = self.dir.join(format!("{TEMP_PREFIX}{hex}"));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok((path, file))
    }
}

/// A stored-but-not-yet-published image. Holds the temp file until
/// [`commit`](StoredImage::commit) renames it into place; dropping it
/// un-committed unlinks, which is what makes every refusal path in the route
/// leak-free without each one remembering to clean up (KTD7).
#[derive(Debug)]
pub struct StoredImage {
    temp: Option<PathBuf>,
    dest: PathBuf,
    kind: ImageKind,
}

impl StoredImage {
    /// The path this image *will* occupy once committed. Available before the
    /// commit so the prompt can be composed while the refusal checks run.
    pub fn dest(&self) -> &Path {
        &self.dest
    }

    /// The sniffed format.
    pub fn kind(&self) -> ImageKind {
        self.kind
    }

    /// Atomically publish the image, returning its final path.
    pub fn commit(mut self) -> Result<PathBuf, DropError> {
        let Some(temp) = self.temp.take() else {
            return Err(DropError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "image already committed or discarded",
            )));
        };
        fs::rename(&temp, &self.dest).map_err(|e| {
            // The rename failed, so the temp file is still there and no longer
            // owned by anything — unlink it now rather than leaving residue for
            // the next startup sweep.
            let _ = fs::remove_file(&temp);
            DropError::Storage(e)
        })?;
        sync_parent_dir(&self.dest);
        Ok(self.dest.clone())
    }

    /// Explicitly drop the image without publishing it. Equivalent to letting
    /// it fall out of scope; spelled out at refusal sites where the intent
    /// deserves to be visible.
    pub fn discard(self) {}
}

impl Drop for StoredImage {
    fn drop(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = fs::remove_file(temp);
        }
    }
}

/// Cap on a drop caption, in chars — the `OTHER_MAX_CHARS` precedent. A caption
/// is a sentence or two typed on a phone; nothing legitimate approaches this.
///
/// Enforced in **two** places, deliberately. The route (U6) bounds the query
/// and refuses an over-cap caption with a 400 rather than silently delivering a
/// truncated one, because a caption clipped mid-sentence changes what the user
/// asked for. This cap is the composer's own backstop, applied through
/// [`crate::feed::io::clean`], so the composed prompt is bounded even if a
/// future caller forgets. In practice the route's refusal fires first.
pub const CAPTION_MAX_CHARS: usize = 512;

/// The fixed framing wrapped around a dropped image (R3).
///
/// The wording is not arbitrary: it must make the agent *open* the file, not
/// treat the path as a string to talk about — a bare path followed by a caption
/// reads ambiguously and would make the whole feature silently useless. This
/// exact phrasing was verified against a live bypass-permissions pane during the
/// U0 premise spike (Claude Code 2.1.219): the agent read the file and reported
/// its contents with no permission prompt. See
/// `docs/notes/2026-07-24-phone-drop-live-check.md`.
///
/// `{path}` is fly-minted (see [`mint_filename`]) and contains no whitespace, so
/// it cannot be split by the surrounding prose.
const PROMPT_FRAMING: &str = "Read the image at {path} — it's a screenshot I dropped from my phone.";

/// Compose the text delivered to the pane: the framing naming the stored image,
/// then the caption if there is one (R1, R3).
///
/// The caption is untrusted text bound for a PTY, so it goes through the
/// existing sanitize → scrub → truncate pipeline
/// ([`crate::feed::io::clean`]) *in that order* before composition — scrubbing
/// before sanitizing would let a zero-width char inside a token-shaped string
/// defeat the prefix match. `clean` returns `None` for a blank result, so a
/// whitespace-only caption is indistinguishable from no caption at all (AE3).
///
/// The result is handed to [`crate::feed::io::paste_payload`], which strips the
/// remaining control characters including ESC — so a caption cannot forge paste
/// markers regardless of what survives here.
pub fn compose_drop_prompt(path: &Path, caption: Option<&str>) -> String {
    let framed = PROMPT_FRAMING.replace("{path}", &path.display().to_string());
    match caption.and_then(|c| crate::feed::io::clean(c, CAPTION_MAX_CHARS)) {
        Some(c) => format!("{framed}\n\n{c}"),
        None => framed,
    }
}

/// Ceiling on the raw (still percent-encoded) query string, in bytes, applied
/// **before** decoding (KTD1).
///
/// Decoding is the one place client text becomes a Rust `String`, so it is
/// bounded first rather than after. The allowance is generous relative to
/// [`CAPTION_MAX_CHARS`] because one caption char can occupy up to 4 UTF-8
/// bytes and each of those up to 3 encoded bytes; the decoded char count is
/// then checked exactly.
pub const QUERY_RAW_MAX_BYTES: usize = 8192;

/// The parsed `POST /drop` query (phone-screenshot-drop U6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropQuery {
    /// The roster `leafKey` being targeted.
    pub agent: String,
    /// The pane id the phone saw on the roster, echoed back for guard one.
    pub pane: u64,
    /// The caption, already percent-decoded; `None` when absent or empty.
    pub caption: Option<String>,
}

/// Why a `POST /drop` query was rejected. All map to `400 badRequest` — they
/// are split for the log line and the tests, not for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Over [`QUERY_RAW_MAX_BYTES`] before decoding.
    TooLong,
    /// `agent` absent or empty.
    MissingAgent,
    /// `pane` absent, empty, or not a `u32`.
    MissingOrBadPane,
    /// A `%` escape that is truncated or not hex, or bytes that do not form
    /// UTF-8 once decoded. **Rejected, never lossily repaired** — a caption
    /// silently mangled into replacement characters is worse than a refusal the
    /// user can act on.
    BadEncoding,
    /// The decoded caption exceeds [`CAPTION_MAX_CHARS`]. Refused rather than
    /// truncated: a caption clipped mid-sentence changes what the user asked.
    CaptionTooLong,
}

/// Parse the `POST /drop` query string (everything after `?`, exclusive).
///
/// Hand-rolled rather than pulled from a crate: this is one small grammar at
/// fly's most security-sensitive listener, and the decode rules below are
/// deliberate rather than inherited.
///
/// **`+` is a literal plus, not a space.** The `application/x-www-form-urlencoded`
/// convention would decode it as a space, but the page builds this query with
/// `encodeURIComponent`, which emits `%20` for a space and leaves `+` untouched.
/// Honoring the form convention would silently turn every plus in a caption into
/// a space.
pub fn parse_drop_query(raw: &str) -> Result<DropQuery, QueryError> {
    if raw.len() > QUERY_RAW_MAX_BYTES {
        return Err(QueryError::TooLong);
    }
    let (mut agent, mut pane, mut caption) = (None, None, None);
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        match k {
            "agent" => agent = Some(percent_decode(v)?),
            "pane" => pane = Some(percent_decode(v)?),
            "caption" => caption = Some(percent_decode(v)?),
            // Unknown parameters are ignored, not refused — a future page
            // revision adding one must not break against an older fly.
            _ => {}
        }
    }

    let agent = agent.filter(|a| !a.is_empty()).ok_or(QueryError::MissingAgent)?;
    let pane = pane
        .ok_or(QueryError::MissingOrBadPane)?
        .parse::<u64>()
        .map_err(|_| QueryError::MissingOrBadPane)?;
    let caption = match caption {
        Some(c) if c.chars().count() > CAPTION_MAX_CHARS => {
            return Err(QueryError::CaptionTooLong)
        }
        Some(c) if c.trim().is_empty() => None,
        other => other,
    };
    Ok(DropQuery {
        agent,
        pane,
        caption,
    })
}

/// Strict `application/x-www-form-urlencoded` decoding to UTF-8. Rejects a
/// truncated or non-hex escape and any byte sequence that is not valid UTF-8,
/// rather than substituting replacement characters.
///
/// `+` decodes to a space, which is the query-string half of the form-encoding
/// the page actually emits: `drop-page.html` builds the query with
/// `URLSearchParams`, whose `set` encodes a space as `+` and a *literal* plus as
/// `%2B` — so the two are lossless together. Percent-decoding alone left every
/// multi-word caption studded with pluses (caught in the first live drop from a
/// phone, 2026-07-27).
fn percent_decode(s: &str) -> Result<String, QueryError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or(QueryError::BadEncoding)?;
            let hi = (hex[0] as char).to_digit(16).ok_or(QueryError::BadEncoding)?;
            let lo = (hex[1] as char).to_digit(16).ok_or(QueryError::BadEncoding)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| QueryError::BadEncoding)
}

/// Decide whether a request's tailnet identity header is acceptable (KTD2).
///
/// Three-valued by design:
/// - no expectation configured ⇒ allow (the shipped default — the check is off
///   until `feed.expectedTailnetLogin` is set);
/// - expectation configured, header absent ⇒ **allow**. `tailscale serve`
///   injects the header, but a request that never crossed the proxy simply has
///   none, and the bearer token — not this — is the boundary;
/// - expectation configured, header present and different ⇒ refuse.
///
/// Comparison is case-insensitive and whitespace-trimmed: logins are email-like
/// and the value may arrive with incidental spacing.
pub fn tailnet_identity_ok(header: Option<&str>, expected: Option<&str>) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|e| !e.is_empty()) else {
        return true;
    };
    match header.map(str::trim).filter(|h| !h.is_empty()) {
        None => true,
        Some(got) => got.eq_ignore_ascii_case(expected),
    }
}

/// What became of one drop delivery attempt (phone-screenshot-drop U5).
///
/// The two failure variants are split rather than collapsed into one boolean
/// because delivery is **two** writes, and the caller must treat them
/// differently (KTD7): a failed paste means nothing reached the pane and the
/// image should be unlinked, while a failed submit means the composed prompt is
/// already sitting in the composer — unlinking then would leave the user
/// hitting Enter at the desk against a path that no longer exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropOutcome {
    /// Pasted and submitted.
    Delivered,
    /// The leaf resolves to no live pane at all — the agent is gone (404).
    UnknownPane,
    /// A live pane exists, but it is not the one the phone targeted: the
    /// session was replaced in the same leaf slot (409 `paneChanged`, AE2).
    PaneChanged,
    /// The right pane, but its foreground process is no longer an agent — a
    /// bare shell, most likely (409 `notAgent`, AE5).
    NotAgent,
    /// Every guard passed but publishing the image failed — an unwritable
    /// directory, a full disk, a failed rename (500 `storageFailed`, AE8).
    /// Nothing reached the pane.
    CommitFailed(String),
    /// The paste write failed; nothing reached the pane. Caller unlinks.
    PasteFailed(String),
    /// The paste landed but the submit did not — either the Enter write failed
    /// or the pre-Enter re-probe found the pane is no longer an agent. The text
    /// is pre-typed and needs an Enter at the machine. **Caller commits the
    /// image**, and says so.
    SubmitIncomplete(String),
}

/// Run the two delivery guards and, if they pass, paste + submit.
///
/// Every dependency is injected so the guard sequence — the part whose failure
/// modes are silent and destructive — is unit-testable without a `PtyManager`,
/// an `AppHandle`, or a running app. `lib.rs` supplies the real implementations.
///
/// **Guard one, pane identity.** `pane_by_leaf` resolves a leaf key to the
/// newest *live* pane, and leaf keys are deliberately stable across respawn, so
/// a leaf whose agent exited and was replaced resolves to the replacement. The
/// phone echoes the pane id it saw on the roster and we compare. Pane ids are
/// monotonic and never reused, so this is *identity*, not freshness — it never
/// moves, and a slow phone flow cannot invalidate it. (That distinction is why
/// this is not the shape that burned `feed-askedat-restamp-409`, which was a
/// freshness stamp the server kept re-stamping under an open dialog.)
///
/// **Guard two, the foreground probe.** Guard one cannot catch the worse case:
/// `claude` exits and leaves a bash prompt in the *same* pane. The id is
/// unchanged and the roster keeps listing it as an agent for up to a poll
/// interval, and since delivery ends in Enter, the path and caption would be
/// **executed as a shell command**.
///
/// **Both are check-then-act, and this does not pretend otherwise.** Neither can
/// run atomically with the write — the foreground probe reads `/proc` with no
/// registry lock held, which is a deliberate lock-discipline invariant in the
/// pty layer, not an oversight to fix here. Worse, delivery is two writes
/// separated by a settle gap: if `claude` exits in that window, bytes already in
/// the tty buffer are inherited by the shell and a delayed Enter would run them.
/// So the probe runs **again** immediately before the Enter and the submit is
/// abandoned if it now fails. The guards narrow the exposure from the roster
/// poll interval to that gap; they do not close it, and AE5 is worded
/// accordingly.
/// **Why `commit` is a parameter rather than the caller's business.** The
/// ordering it enforces is not stylistic. Every refusal check must run before
/// the image is published (KTD7 — a refusal must leave no residue), but the
/// image must exist before any text reaches the pane, because the pasted prompt
/// names a path the agent is about to be asked to read. Committing after the
/// writes would race the agent against the rename; committing before the guards
/// would leave a published image behind every `paneChanged`. So the publish
/// happens *between* them, and the only way to guarantee that is to own the
/// sequence here.
pub fn deliver_with_guards(
    expect_pane: u64,
    text: &str,
    resolve_pane: impl Fn() -> Option<u64>,
    is_agent: impl Fn(u64) -> bool,
    write: impl FnMut(u64, &[u8]) -> Result<(), String>,
    settle: impl Fn(),
    commit: impl FnOnce() -> Result<(), String>,
) -> DropOutcome {
    deliver_with_guards_verified(
        expect_pane,
        text,
        resolve_pane,
        is_agent,
        write,
        settle,
        commit,
        |_| {},
        |_| None,
        |_| {},
    )
}

/// Verified-submit polling knobs (tmux-substrate U6/KTD5, the ga-bwm lesson):
/// after the Enter, wait for the pane's output ring to grow — a submitted
/// turn produces output, a still-parked composer produces none — and re-send
/// the Enter only while the ring stays static (a growing ring means the turn
/// started; a second Enter must never reach a busy agent). Unconfirmed after
/// the attempts is logged, NOT surfaced as a refusal: callers would retry the
/// whole delivery and double-paste (Gas City preserved the same contract).
const SUBMIT_CONFIRM_ATTEMPTS: usize = 3;
const SUBMIT_CONFIRM_POLLS: usize = 4;
const SUBMIT_CONFIRM_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// [`deliver_with_guards`] plus the U6 seams: `wake` runs after commit and
/// before the paste (the detached-TUI SIGWINCH insurance — a no-op closure
/// for PTY-backed panes), `output_seq` samples the pane's output-ring
/// sequence (`None` = no signal available ⇒ single-Enter legacy behavior),
/// and `sleep` is injected so tests drive the confirm loop without a clock.
#[allow(clippy::too_many_arguments)]
pub fn deliver_with_guards_verified(
    expect_pane: u64,
    text: &str,
    resolve_pane: impl Fn() -> Option<u64>,
    is_agent: impl Fn(u64) -> bool,
    mut write: impl FnMut(u64, &[u8]) -> Result<(), String>,
    settle: impl Fn(),
    commit: impl FnOnce() -> Result<(), String>,
    wake: impl Fn(u64),
    mut output_seq: impl FnMut(u64) -> Option<u64>,
    sleep: impl Fn(std::time::Duration),
) -> DropOutcome {
    let Some(pane) = resolve_pane() else {
        return DropOutcome::UnknownPane;
    };
    if pane != expect_pane {
        return DropOutcome::PaneChanged;
    }
    if !is_agent(pane) {
        return DropOutcome::NotAgent;
    }

    // Last point at which nothing has been published and nothing has been
    // typed. Past here the image is on disk under its final name.
    if let Err(e) = commit() {
        return DropOutcome::CommitFailed(e);
    }

    // U6: wake a detached TUI's event loop before bytes arrive (some
    // versions/providers don't process stdin without a terminal event).
    wake(pane);

    if let Err(e) = write(pane, &crate::feed::io::paste_payload(text)) {
        return DropOutcome::PasteFailed(e);
    }
    settle();
    // The residual-race mitigation. Abandoning here leaves unsubmitted text in
    // a shell, which is recoverable; sending the Enter anyway would execute it.
    if !is_agent(pane) {
        return DropOutcome::SubmitIncomplete(
            "the pane stopped running an agent before the text could be submitted".into(),
        );
    }
    // Baseline AFTER the settle so the paste's own echo has largely landed;
    // a late echo can only false-confirm, which degrades to the legacy
    // single-Enter behavior (the insurance fails open).
    let baseline = output_seq(pane);
    if let Err(e) = write(pane, crate::feed::io::SUBMIT) {
        return DropOutcome::SubmitIncomplete(e);
    }
    let Some(baseline) = baseline else {
        return DropOutcome::Delivered; // no signal ⇒ legacy behavior
    };
    for attempt in 0..SUBMIT_CONFIRM_ATTEMPTS {
        for _ in 0..SUBMIT_CONFIRM_POLLS {
            match output_seq(pane) {
                Some(seq) if seq != baseline => return DropOutcome::Delivered,
                None => return DropOutcome::Delivered, // signal lost mid-loop
                _ => sleep(SUBMIT_CONFIRM_POLL),
            }
        }
        // Ring static ⇒ the composer is still parked; the Enter was likely
        // swallowed racing the paste (ga-bwm). Safe to re-send: a busy agent
        // implies a growing ring, which exits above.
        if attempt + 1 < SUBMIT_CONFIRM_ATTEMPTS {
            if !is_agent(pane) {
                return DropOutcome::SubmitIncomplete(
                    "the pane stopped running an agent during submit confirmation".into(),
                );
            }
            if let Err(e) = write(pane, crate::feed::io::SUBMIT) {
                return DropOutcome::SubmitIncomplete(e);
            }
        }
    }
    log::warn!(
        "drop/peer delivery to pane {pane}: submit unconfirmed after {SUBMIT_CONFIRM_ATTEMPTS} Enters (text may sit drafted)"
    );
    DropOutcome::Delivered
}

/// Fill 8 bytes from the thread CSPRNG (the `hooks::token` / `config` idiom).
fn random_bytes() -> [u8; 8] {
    use rand::RngCore;
    let mut raw = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut raw);
    raw
}

/// Read until `buf` is full or the reader ends, returning how many bytes
/// landed. A single `read` may return fewer bytes than asked for even when more
/// are coming, so the sniff must loop or it will abstain on a slow socket.
fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Remove fly-prefixed temp files older than [`TEMP_STALE_AFTER`] (KTD7).
///
/// The unlink-on-drop above covers early returns, not SIGKILL, a panic-abort, or
/// the user quitting fly mid-upload. In those a partial temp survives in a
/// directory the user browses, so it gets cleaned at next start. Entirely
/// best-effort — a sweep failure must never keep the store from constructing.
///
/// Only the fly prefix is ever considered: this directory belongs to the user,
/// and a sweep that deleted anything else would be a data-loss bug.
fn sweep_stale_temps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > TEMP_STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    /// Fixtures live in `src-tauri/tests/fixtures/images/`. PNG/JPEG/GIF/WebP/
    /// AVIF are genuine encoder output (ffmpeg); the HEIC fixture is a
    /// hand-built ISO-BMFF `ftyp` box, because no encoder on this machine muxes
    /// HEIC — its first 16 bytes are byte-identical to a real HEIC's, which is
    /// the whole of what the sniffer reads.
    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/images")
            .join(name);
        fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
    }

    #[test]
    fn tilde_slash_expands_against_home() {
        let got = expand_tilde("~/projects/inbox", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(expand_tilde("~", Some(&home())).unwrap(), home());
        // `~/` is the same place — a trailing slash is not a subdirectory.
        assert_eq!(expand_tilde("~/", Some(&home())).unwrap(), home());
    }

    #[test]
    fn absolute_path_passes_through_unchanged() {
        let got = expand_tilde("/srv/shots", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/srv/shots"));
    }

    #[test]
    fn absolute_path_does_not_need_home() {
        assert!(expand_tilde("/srv/shots", None).is_ok());
    }

    /// The case the whole helper exists for: a bare relative path must be
    /// refused, never resolved against the process cwd (`/` for a GUI launched
    /// from a desktop file).
    #[test]
    fn bare_relative_path_is_rejected() {
        assert_eq!(expand_tilde("inbox", Some(&home())), Err(DropDirError::Relative));
        assert_eq!(
            expand_tilde("./inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
        assert_eq!(
            expand_tilde("../inbox", Some(&home())),
            Err(DropDirError::Relative)
        );
    }

    #[test]
    fn empty_and_whitespace_are_rejected() {
        assert_eq!(expand_tilde("", Some(&home())), Err(DropDirError::Empty));
        assert_eq!(expand_tilde("   ", Some(&home())), Err(DropDirError::Empty));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let got = expand_tilde("  ~/inbox  ", Some(&home())).unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/inbox"));
    }

    #[test]
    fn foreign_home_is_rejected_not_guessed() {
        assert_eq!(
            expand_tilde("~root/inbox", Some(&home())),
            Err(DropDirError::ForeignHome)
        );
    }

    #[test]
    fn tilde_without_home_is_rejected() {
        assert_eq!(expand_tilde("~/inbox", None), Err(DropDirError::NoHome));
        assert_eq!(expand_tilde("~", None), Err(DropDirError::NoHome));
    }

    #[test]
    fn unconfigured_defaults_under_the_data_root() {
        let got = resolve_drop_dir(None, Some(&home()), Path::new("/data/fly")).unwrap();
        assert_eq!(got, PathBuf::from("/data/fly/inbox"));
    }

    #[test]
    fn configured_value_wins_over_the_data_root() {
        let got =
            resolve_drop_dir(Some("~/projects/inbox"), Some(&home()), Path::new("/data/fly"))
                .unwrap();
        assert_eq!(got, PathBuf::from("/home/tester/projects/inbox"));
    }

    #[test]
    fn configured_bad_shape_propagates_rather_than_falling_back() {
        let got = resolve_drop_dir(Some("inbox"), Some(&home()), Path::new("/data/fly"));
        assert_eq!(got, Err(DropDirError::Relative));
    }

    // ---- U2: sniffing ----

    #[test]
    fn every_supported_format_sniffs_from_a_real_fixture() {
        for (file, want) in [
            ("sample.png", ImageKind::Png),
            ("sample.jpg", ImageKind::Jpeg),
            ("sample.gif", ImageKind::Gif),
            ("sample.webp", ImageKind::Webp),
            ("sample.heic", ImageKind::Heic),
            ("sample.avif", ImageKind::Avif),
        ] {
            assert_eq!(sniff_image(&fixture(file)), Some(want), "{file}");
        }
    }

    #[test]
    fn non_images_abstain_rather_than_guessing() {
        assert_eq!(sniff_image(b""), None, "empty");
        assert_eq!(sniff_image(b"\x89PN"), None, "3-byte truncated PNG prefix");
        assert_eq!(sniff_image(&fixture("notimage.txt")), None, "text");
        assert_eq!(sniff_image(&fixture("notimage.pdf")), None, "pdf");
    }

    /// The near-miss the WHATWG pattern exists for: RIFF is a container family,
    /// so matching the `RIFF` prefix alone would accept a WAV as a WebP.
    #[test]
    fn riff_that_is_not_webp_abstains() {
        let wav = fixture("riff-not-webp.wav");
        assert!(wav.starts_with(b"RIFF"), "fixture really is RIFF");
        assert_eq!(sniff_image(&wav), None);
    }

    /// An ISO-BMFF file whose `ftyp` brand is neither HEIC nor AVIF abstains
    /// rather than being stored under a guessed extension.
    #[test]
    fn unknown_ftyp_brand_abstains() {
        let mut mp4 = Vec::from(&b"\x00\x00\x00\x20ftypisom"[..]);
        mp4.extend_from_slice(&[0u8; 8]);
        assert_eq!(sniff_image(&mp4), None);
    }

    /// R16 in one assertion: the bytes decide, not the declared content type.
    #[test]
    fn jpeg_bytes_win_over_a_png_content_type_claim() {
        // The route never consults the declared type at all, so the strongest
        // form of this test is that JPEG bytes yield a `.jpg` extension.
        let kind = sniff_image(&fixture("sample.jpg")).unwrap();
        assert_eq!(kind.extension(), "jpg");
    }

    // ---- U2: filename minting ----

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn mint_filename_is_deterministic_under_fixed_inputs() {
        let name = mint_filename(
            at("2026-07-25T00:31:07Z"),
            [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04],
            ImageKind::Png,
        );
        assert_eq!(name, "20260725T003107Z-deadbeef01020304.png");
    }

    #[test]
    fn mint_filename_differs_on_different_random_bytes() {
        let now = at("2026-07-25T00:31:07Z");
        let a = mint_filename(now, [0; 8], ImageKind::Png);
        let b = mint_filename(now, [1; 8], ImageKind::Png);
        assert_ne!(a, b, "same instant, different randomness");
    }

    /// The invariant that lets the minted name be pasted into a prompt and used
    /// as a path component without quoting or sanitizing (R16, U3).
    #[test]
    fn minted_name_has_no_separator_traversal_or_whitespace() {
        for kind in [
            ImageKind::Png,
            ImageKind::Jpeg,
            ImageKind::Gif,
            ImageKind::Webp,
            ImageKind::Heic,
            ImageKind::Avif,
        ] {
            let name = mint_filename(at("2026-07-25T00:31:07Z"), [0xab; 8], kind);
            assert!(!name.contains('/'), "{name}");
            assert!(!name.contains(".."), "{name}");
            assert!(!name.contains(char::is_whitespace), "{name}");
            assert!(name.ends_with(kind.extension()), "{name}");
        }
    }

    // ---- U2: storage ----

    fn store_of(dir: &Path) -> DropStore {
        DropStore::new(dir).expect("store constructs")
    }

    /// Files left in the directory, excluding fly's own temp files.
    fn published(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with(TEMP_PREFIX))
            .collect();
        v.sort();
        v
    }

    /// Every file, temp included — the leak check.
    fn all_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn store_then_commit_publishes_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let img = store.store(&mut bytes.as_slice(), 1 << 20).unwrap();
        assert_eq!(img.kind(), ImageKind::Png);
        let path = img.commit().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes, "content round-trips");
        assert_eq!(published(tmp.path()).len(), 1);
        assert_eq!(all_entries(tmp.path()).len(), 1, "no temp residue");
    }

    #[test]
    fn commit_writes_0600_into_a_0700_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        let path = img.commit().unwrap();
        let fmode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dmode = fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "file is owner-only");
        assert_eq!(dmode, 0o700, "directory is owner-only");
    }

    #[test]
    fn stored_path_is_inside_the_configured_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        let path = img.commit().unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        assert!(canonical.starts_with(store.dir()), "{canonical:?}");
    }

    /// A body exactly at the cap is accepted — the boundary is inclusive.
    #[test]
    fn body_exactly_at_the_cap_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let img = store
            .store(&mut bytes.as_slice(), bytes.len() as u64)
            .unwrap();
        img.commit().unwrap();
        assert_eq!(published(tmp.path()).len(), 1);
    }

    /// One byte over is refused, and nothing survives — not even a temp file.
    #[test]
    fn body_one_byte_over_the_cap_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let cap = bytes.len() as u64 - 1;
        let err = store.store(&mut bytes.as_slice(), cap).unwrap_err();
        assert!(matches!(err, DropError::Oversize), "{err:?}");
        assert!(all_entries(tmp.path()).is_empty(), "no residue");
    }

    /// Oversize is detected without buffering the whole body: the reader is
    /// bounded at cap+1, so a body far larger than the cap stops early.
    #[test]
    fn oversize_stops_reading_shortly_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let mut body = fixture("sample.png");
        body.resize(4 * 1024 * 1024, 0);
        let mut counting = CountingReader {
            inner: body.as_slice(),
            read: 0,
        };
        let err = store.store(&mut counting, 64 * 1024).unwrap_err();
        assert!(matches!(err, DropError::Oversize));
        assert!(
            counting.read < 4 * 1024 * 1024,
            "stopped early, read {} of 4 MiB",
            counting.read
        );
        assert!(all_entries(tmp.path()).is_empty());
    }

    #[test]
    fn non_image_body_is_refused_before_any_file_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let err = store
            .store(&mut fixture("notimage.pdf").as_slice(), 1 << 20)
            .unwrap_err();
        assert!(matches!(err, DropError::BadFormat), "{err:?}");
        assert!(all_entries(tmp.path()).is_empty(), "nothing was created");
    }

    /// The invariant behind every refusal path in the route: an uncommitted
    /// handle cleans up after itself, so a caller's early return cannot orphan
    /// a partial image in a directory the user browses.
    #[test]
    fn dropping_an_uncommitted_image_unlinks_its_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        {
            let img = store
                .store(&mut fixture("sample.png").as_slice(), 1 << 20)
                .unwrap();
            assert_eq!(all_entries(tmp.path()).len(), 1, "temp exists while held");
            assert!(all_entries(tmp.path())[0].starts_with(TEMP_PREFIX));
            drop(img);
        }
        assert!(all_entries(tmp.path()).is_empty(), "unlinked on drop");
    }

    #[test]
    fn explicit_discard_also_unlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let img = store
            .store(&mut fixture("sample.png").as_slice(), 1 << 20)
            .unwrap();
        img.discard();
        assert!(all_entries(tmp.path()).is_empty());
    }

    #[test]
    fn storing_into_a_read_only_directory_errors_rather_than_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let got = store.store(&mut fixture("sample.png").as_slice(), 1 << 20);
        // Restore before the assert so the TempDir can always clean up.
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(got, Err(DropError::Storage(_))), "expected storage error");
    }

    /// A slow reader that hands back one byte at a time must still sniff — the
    /// header read loops rather than trusting a single `read` to fill it.
    #[test]
    fn sniff_survives_a_reader_that_dribbles_one_byte_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_of(tmp.path());
        let bytes = fixture("sample.png");
        let mut dribble = DribbleReader {
            inner: bytes.as_slice(),
        };
        let img = store.store(&mut dribble, 1 << 20).unwrap();
        assert_eq!(img.kind(), ImageKind::Png);
        let path = img.commit().unwrap();
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    // ---- U2: startup sweep (KTD7) ----

    #[test]
    fn construction_sweeps_stale_temps_but_spares_user_files_and_fresh_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let stale = tmp.path().join(format!("{TEMP_PREFIX}0000000000000000"));
        let fresh = tmp.path().join(format!("{TEMP_PREFIX}1111111111111111"));
        let mine = tmp.path().join("holiday-photo.png");
        for p in [&stale, &fresh, &mine] {
            fs::write(p, b"x").unwrap();
        }
        // Age only the stale one past the sweep threshold.
        let old = SystemTime::now() - TEMP_STALE_AFTER - Duration::from_secs(60);
        set_mtime(&stale, old);

        let _store = store_of(tmp.path());

        assert!(!stale.exists(), "stale fly temp was swept");
        assert!(fresh.exists(), "a fresh temp may be an upload in flight");
        assert!(
            mine.exists(),
            "the sweep must never touch a file that is not fly's — this is the \
             user's own directory (KTD4)"
        );
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let ft = filetime_from(when);
        let times = [ft, ft];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path and `times` is a 2-element
        // timeval array, which is exactly what `utimes(2)` expects.
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {path:?}");
    }

    fn filetime_from(when: SystemTime) -> libc::timeval {
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        }
    }

    // ---- U3: prompt composition ----

    fn prompt(caption: Option<&str>) -> String {
        compose_drop_prompt(
            Path::new("/home/tester/inbox/20260725T003107Z-deadbeef01020304.png"),
            caption,
        )
    }

    #[test]
    fn prompt_names_the_path_as_an_image_to_read_and_carries_the_caption() {
        let out = prompt(Some("the login button overlaps the header on iOS"));
        assert!(out.contains("20260725T003107Z-deadbeef01020304.png"), "{out}");
        assert!(
            out.contains("the login button overlaps the header on iOS"),
            "{out}"
        );
        // The framing must direct the agent to *open* the file, not merely
        // mention a path — this is the whole feature (R3, verified in U0).
        assert!(out.starts_with("Read the image at "), "{out}");
    }

    #[test]
    fn prompt_without_a_caption_is_the_framing_alone() {
        let out = prompt(None);
        assert!(out.starts_with("Read the image at "), "{out}");
        assert!(!out.trim().is_empty());
        assert!(!out.contains("\n\n"), "no dangling blank line: {out:?}");
    }

    /// AE3: a caption of only whitespace must be treated as no caption, not as
    /// an empty line the agent has to interpret.
    #[test]
    fn whitespace_only_caption_matches_the_no_caption_case() {
        assert_eq!(prompt(Some("   \n\t  ")), prompt(None));
        assert_eq!(prompt(Some("")), prompt(None));
    }

    /// The caption reaches a PTY, so no ESC may survive — otherwise it could
    /// forge the bracketed-paste end marker or smuggle a terminal sequence.
    #[test]
    fn caption_escape_bytes_do_not_survive() {
        let out = prompt(Some("before\x1b[201~after\x07end"));
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(!out.contains('\x07'), "{out:?}");
        assert!(out.contains("beforeafter") || out.contains("before"), "{out:?}");
    }

    #[test]
    fn caption_secrets_are_scrubbed() {
        let out = prompt(Some("token is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(
            !out.contains("sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            "{out}"
        );
    }

    /// Truncation runs *after* scrubbing, so a secret straddling the cap is
    /// masked before its tail is cut (the `io::clean` ordering contract).
    #[test]
    fn overlong_caption_is_truncated_after_scrubbing() {
        let long = "x".repeat(CAPTION_MAX_CHARS + 500);
        let out = prompt(Some(&long));
        assert!(out.contains('…'), "truncation marker present: {out:?}");
        assert!(out.chars().count() < long.chars().count());

        // A secret placed right at the boundary is scrubbed, not half-cut.
        let mut straddle = "y".repeat(CAPTION_MAX_CHARS - 10);
        straddle.push_str("sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
        let out = prompt(Some(&straddle));
        assert!(!out.contains("sk-ant-api03-BBBB"), "{out}");
    }

    /// Bracketed paste exists precisely so a multi-line caption lands as one
    /// composer message — newlines must survive composition.
    #[test]
    fn multiline_caption_keeps_its_newlines() {
        let out = prompt(Some("line one\nline two"));
        assert!(out.contains("line one\nline two"), "{out:?}");
    }

    /// The path is emitted unquoted, so a minted name containing whitespace
    /// would be split by the surrounding prose. `mint_filename` guarantees it
    /// cannot; this pins the dependency between the two.
    #[test]
    fn composed_path_is_a_single_whitespace_free_token() {
        let name = mint_filename(at("2026-07-25T00:31:07Z"), [0x5a; 8], ImageKind::Png);
        let out = compose_drop_prompt(&PathBuf::from("/home/tester/inbox").join(&name), None);
        let found = out
            .split_whitespace()
            .find(|t| t.contains(&name))
            .expect("the path appears as one token");
        assert!(found.ends_with(&name), "{found}");
    }

    // ---- U6: query parsing ----

    #[test]
    fn a_well_formed_query_parses_all_three_parameters() {
        let q = parse_drop_query("agent=leaf-12&pane=7&caption=login%20button%20overlaps").unwrap();
        assert_eq!(q.agent, "leaf-12");
        assert_eq!(q.pane, 7);
        assert_eq!(q.caption.as_deref(), Some("login button overlaps"));
    }

    #[test]
    fn a_caption_is_optional_and_blank_is_absent() {
        assert_eq!(parse_drop_query("agent=l&pane=1").unwrap().caption, None);
        assert_eq!(parse_drop_query("agent=l&pane=1&caption=").unwrap().caption, None);
        assert_eq!(
            parse_drop_query("agent=l&pane=1&caption=%20%20").unwrap().caption,
            None,
            "whitespace-only is absent, matching compose_drop_prompt"
        );
    }

    #[test]
    fn a_missing_or_unparseable_pane_is_refused() {
        // R14 depends on the echo being present — defaulting it would silently
        // disable guard one.
        assert_eq!(
            parse_drop_query("agent=l"),
            Err(QueryError::MissingOrBadPane)
        );
        assert_eq!(
            parse_drop_query("agent=l&pane="),
            Err(QueryError::MissingOrBadPane)
        );
        assert_eq!(
            parse_drop_query("agent=l&pane=abc"),
            Err(QueryError::MissingOrBadPane)
        );
        assert_eq!(
            parse_drop_query("agent=l&pane=-1"),
            Err(QueryError::MissingOrBadPane)
        );
    }

    #[test]
    fn a_missing_agent_is_refused() {
        assert_eq!(parse_drop_query("pane=1"), Err(QueryError::MissingAgent));
        assert_eq!(
            parse_drop_query("agent=&pane=1"),
            Err(QueryError::MissingAgent)
        );
    }

    #[test]
    fn invalid_percent_encoding_is_refused_not_repaired() {
        for bad in [
            "agent=l&pane=1&caption=%",
            "agent=l&pane=1&caption=%A",
            "agent=l&pane=1&caption=%ZZ",
            "agent=l&pane=1&caption=%FF%FE", // decodes to invalid UTF-8
        ] {
            assert_eq!(parse_drop_query(bad), Err(QueryError::BadEncoding), "{bad}");
        }
    }

    #[test]
    fn multibyte_captions_survive_the_round_trip() {
        let q = parse_drop_query("agent=l&pane=1&caption=caf%C3%A9%20%F0%9F%93%B7").unwrap();
        assert_eq!(q.caption.as_deref(), Some("café 📷"));
    }

    /// `+` decodes to a space. The page builds its query with `URLSearchParams`,
    /// which emits `+` for a space — decoding it as a literal plus mangled every
    /// multi-word caption ("What+is+in+the+picture?" on the first phone drop).
    #[test]
    fn plus_decodes_to_a_space() {
        let q = parse_drop_query("agent=l&pane=1&caption=a+b").unwrap();
        assert_eq!(q.caption.as_deref(), Some("a b"));
    }

    /// The other half of that contract: a caption the user genuinely typed with
    /// a plus in it survives, because `URLSearchParams` escapes it as `%2B`.
    #[test]
    fn an_escaped_plus_survives_as_a_literal_plus() {
        let q = parse_drop_query("agent=l&pane=1&caption=c%2B%2B+build+broke").unwrap();
        assert_eq!(q.caption.as_deref(), Some("c++ build broke"));
    }

    #[test]
    fn an_over_cap_caption_is_refused_rather_than_truncated() {
        let long = "x".repeat(CAPTION_MAX_CHARS + 1);
        let q = format!("agent=l&pane=1&caption={long}");
        assert_eq!(parse_drop_query(&q), Err(QueryError::CaptionTooLong));

        // Exactly at the cap is fine.
        let at = "x".repeat(CAPTION_MAX_CHARS);
        let q = format!("agent=l&pane=1&caption={at}");
        assert_eq!(
            parse_drop_query(&q).unwrap().caption.unwrap().chars().count(),
            CAPTION_MAX_CHARS
        );
    }

    #[test]
    fn an_oversized_raw_query_is_refused_before_decoding() {
        let q = format!("agent=l&pane=1&caption={}", "%20".repeat(4000));
        assert!(q.len() > QUERY_RAW_MAX_BYTES);
        assert_eq!(parse_drop_query(&q), Err(QueryError::TooLong));
    }

    #[test]
    fn unknown_parameters_are_ignored_for_forward_compatibility() {
        let q = parse_drop_query("agent=l&pane=1&future=whatever").unwrap();
        assert_eq!(q.agent, "l");
    }

    #[test]
    fn a_leaf_key_containing_encoded_characters_decodes() {
        let q = parse_drop_query("agent=ws-1%2Ftab-1%2Fleaf-1&pane=1").unwrap();
        assert_eq!(q.agent, "ws-1/tab-1/leaf-1");
    }

    // ---- U6: tailnet identity (KTD2) ----

    #[test]
    fn identity_check_is_off_until_an_expectation_is_configured() {
        assert!(tailnet_identity_ok(None, None));
        assert!(tailnet_identity_ok(Some("anyone@example.com"), None));
        assert!(tailnet_identity_ok(Some("anyone@example.com"), Some("  ")));
    }

    #[test]
    fn a_matching_identity_passes_and_a_mismatched_one_is_refused() {
        assert!(tailnet_identity_ok(
            Some("evan@example.com"),
            Some("evan@example.com")
        ));
        assert!(!tailnet_identity_ok(
            Some("someone-else@example.com"),
            Some("evan@example.com")
        ));
    }

    /// Absence is not a refusal: the token remains the boundary, and a request
    /// that never crossed the proxy simply carries no header.
    #[test]
    fn an_absent_identity_header_is_allowed_even_when_configured() {
        assert!(tailnet_identity_ok(None, Some("evan@example.com")));
        assert!(tailnet_identity_ok(Some(""), Some("evan@example.com")));
    }

    #[test]
    fn identity_comparison_ignores_case_and_surrounding_space() {
        assert!(tailnet_identity_ok(
            Some(" Evan@Example.COM "),
            Some("evan@example.com")
        ));
    }

    // ---- U5: delivery guards ----
    //
    // Written before the delivery path itself (the plan's execution note):
    // both guards fail *silently and destructively* — a missed pane-identity
    // check delivers into a stranger's session, a missed foreground check
    // executes the caption as a shell command — so a regression that skips one
    // must fail loudly here rather than being discovered in a pane.

    /// A recording fake for the delivery seam: logs every PTY write and lets a
    /// test script the pane resolution, the agent probe per call, and write
    /// failures.
    #[derive(Default)]
    struct FakePane {
        resolved: Option<u64>,
        /// Answers for successive `is_agent` probes, consumed front to back;
        /// the last value repeats once exhausted.
        agent_probes: std::cell::RefCell<Vec<bool>>,
        /// Write index (0-based) that should fail, if any.
        fail_write: Option<usize>,
        writes: std::cell::RefCell<Vec<Vec<u8>>>,
        settles: std::cell::Cell<usize>,
        /// Set when `commit` runs, recording how many writes had happened by
        /// then — the ordering assertion.
        committed_after_writes: std::cell::Cell<Option<usize>>,
        commit_fails: bool,
    }

    impl FakePane {
        fn agent(resolved: u64) -> Self {
            Self {
                resolved: Some(resolved),
                agent_probes: std::cell::RefCell::new(vec![true]),
                ..Default::default()
            }
        }
        fn probe(&self, _pane: u64) -> bool {
            let mut p = self.agent_probes.borrow_mut();
            if p.len() > 1 {
                p.remove(0)
            } else {
                *p.first().unwrap_or(&false)
            }
        }
        fn write(&self, _pane: u64, bytes: &[u8]) -> Result<(), String> {
            let mut w = self.writes.borrow_mut();
            let idx = w.len();
            if self.fail_write == Some(idx) {
                return Err("EIO".into());
            }
            w.push(bytes.to_vec());
            Ok(())
        }
        fn run(&self, expect_pane: u64, text: &str) -> DropOutcome {
            deliver_with_guards(
                expect_pane,
                text,
                || self.resolved,
                |p| self.probe(p),
                |p, b| self.write(p, b),
                || {
                    self.settles.set(self.settles.get() + 1);
                },
                || {
                    self.committed_after_writes
                        .set(Some(self.writes.borrow().len()));
                    if self.commit_fails {
                        Err("ENOSPC".into())
                    } else {
                        Ok(())
                    }
                },
            )
        }
        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes.borrow().clone()
        }
    }

    /// U6 verified submit: a growing output ring confirms on the first
    /// poll — exactly one Enter, no sleeps beyond the first poll.
    #[test]
    fn verified_submit_confirms_on_ring_growth_without_reenter() {
        let writes = std::cell::RefCell::new(Vec::<Vec<u8>>::new());
        let seq = std::cell::Cell::new(100u64);
        let out = deliver_with_guards_verified(
            7,
            "hi",
            || Some(7),
            |_| true,
            |_, b| {
                writes.borrow_mut().push(b.to_vec());
                Ok(())
            },
            || {},
            || Ok(()),
            |_| {},
            |_| {
                let v = seq.get();
                seq.set(v + 50); // ring grows between samples
                Some(v)
            },
            |_| {},
        );
        assert_eq!(out, DropOutcome::Delivered);
        let w = writes.borrow();
        assert_eq!(w.len(), 2, "paste + exactly one Enter, got {}", w.len());
    }

    /// A parked composer (static ring) draws the ga-bwm re-Enters — capped,
    /// and still reported Delivered (logged, never a refusal: callers would
    /// double-paste).
    #[test]
    fn verified_submit_reenters_while_ring_static_then_caps() {
        let writes = std::cell::RefCell::new(Vec::<Vec<u8>>::new());
        let out = deliver_with_guards_verified(
            7,
            "hi",
            || Some(7),
            |_| true,
            |_, b| {
                writes.borrow_mut().push(b.to_vec());
                Ok(())
            },
            || {},
            || Ok(()),
            |_| {},
            |_| Some(42), // never grows
            |_| {},
        );
        assert_eq!(out, DropOutcome::Delivered);
        let w = writes.borrow();
        let enters = w.iter().filter(|b| b.as_slice() == crate::feed::io::SUBMIT).count();
        assert_eq!(enters, SUBMIT_CONFIRM_ATTEMPTS, "capped re-Enters");
    }

    /// No ring signal (None) is the legacy contract: one Enter, done —
    /// which is also why every pre-U6 test in this module is unchanged.
    #[test]
    fn verified_submit_without_signal_is_legacy_single_enter() {
        let writes = std::cell::RefCell::new(Vec::<Vec<u8>>::new());
        let out = deliver_with_guards_verified(
            7,
            "hi",
            || Some(7),
            |_| true,
            |_, b| {
                writes.borrow_mut().push(b.to_vec());
                Ok(())
            },
            || {},
            || Ok(()),
            |_| {},
            |_| None,
            |_| panic!("no polling without a signal"),
        );
        assert_eq!(out, DropOutcome::Delivered);
        assert_eq!(writes.borrow().len(), 2);
    }

    /// The agent dying mid-confirmation aborts the re-Enter (a shell must
    /// never receive it) and reports SubmitIncomplete.
    #[test]
    fn verified_submit_aborts_reenter_when_agent_dies() {
        let agent_alive = std::cell::Cell::new(true);
        let polls = std::cell::Cell::new(0u32);
        let out = deliver_with_guards_verified(
            7,
            "hi",
            || Some(7),
            |_| agent_alive.get(),
            |_, _| Ok(()),
            || {},
            || Ok(()),
            |_| {},
            |_| {
                polls.set(polls.get() + 1);
                if polls.get() > 2 {
                    agent_alive.set(false);
                }
                Some(42)
            },
            |_| {},
        );
        assert!(matches!(out, DropOutcome::SubmitIncomplete(_)));
    }

    /// The wake seam fires after commit and before the paste.
    #[test]
    fn wake_runs_before_paste() {
        let order = std::cell::RefCell::new(Vec::<&'static str>::new());
        let _ = deliver_with_guards_verified(
            7,
            "hi",
            || Some(7),
            |_| true,
            |_, _| {
                order.borrow_mut().push("write");
                Ok(())
            },
            || {},
            || {
                order.borrow_mut().push("commit");
                Ok(())
            },
            |_| order.borrow_mut().push("wake"),
            |_| None,
            |_| {},
        );
        assert_eq!(&order.borrow()[..3], &["commit", "wake", "write"]);
    }

    #[test]
    fn delivery_with_matching_pane_and_agent_foreground_succeeds() {
        let f = FakePane::agent(7);
        assert_eq!(f.run(7, "hello"), DropOutcome::Delivered);
        let w = f.writes();
        assert_eq!(w.len(), 2, "paste and Enter are two separate writes");
        assert!(
            String::from_utf8_lossy(&w[0]).contains("hello"),
            "first write carries the text"
        );
        assert_eq!(w[1], crate::feed::io::SUBMIT, "second write is the Enter");
        assert_eq!(f.settles.get(), 1, "one settle gap between them");
    }

    /// The ordering KTD7 forces, and the reason it is owned by
    /// `deliver_with_guards` rather than left to the caller: the image must be
    /// published *after* every refusal check (so a refusal leaves no residue)
    /// but *before* any text reaches the pane (so the agent is never told to
    /// read a path that has not been renamed into place yet).
    #[test]
    fn the_image_is_committed_after_the_guards_and_before_the_first_write() {
        let f = FakePane::agent(7);
        assert_eq!(f.run(7, "hello"), DropOutcome::Delivered);
        assert_eq!(
            f.committed_after_writes.get(),
            Some(0),
            "commit ran with zero writes behind it"
        );
    }

    #[test]
    fn a_refused_drop_never_commits_the_image() {
        for f in [
            FakePane::agent(9), // pane changed
            FakePane {
                resolved: None,
                ..Default::default()
            }, // unknown
            FakePane {
                resolved: Some(7),
                agent_probes: std::cell::RefCell::new(vec![false]),
                ..Default::default()
            }, // not an agent
        ] {
            f.run(7, "hello");
            assert_eq!(
                f.committed_after_writes.get(),
                None,
                "a refusal must leave no published image"
            );
        }
    }

    /// AE8: publishing failed after the guards passed. Nothing was typed, and
    /// the outcome is distinguishable from a refusal.
    #[test]
    fn a_failed_commit_reports_storage_failure_and_types_nothing() {
        let f = FakePane {
            resolved: Some(7),
            agent_probes: std::cell::RefCell::new(vec![true]),
            commit_fails: true,
            ..Default::default()
        };
        let out = f.run(7, "hello");
        assert!(matches!(out, DropOutcome::CommitFailed(_)), "got {out:?}");
        assert!(f.writes().is_empty(), "nothing reached the pane");
    }

    /// AE2 — the session was replaced in the same leaf slot. Pane ids are
    /// monotonic, so the resolved id is *higher* than the one the phone echoed.
    #[test]
    fn a_replaced_pane_is_refused_as_pane_changed_with_no_write() {
        let f = FakePane::agent(9);
        assert_eq!(f.run(7, "hello"), DropOutcome::PaneChanged);
        assert!(f.writes().is_empty(), "nothing reached the PTY");
    }

    #[test]
    fn a_leaf_with_no_live_pane_is_unknown_not_pane_changed() {
        // These must stay distinguishable: "the agent is gone" (404) and "a
        // different agent is here now" (409) mean different things to the user.
        let f = FakePane {
            resolved: None,
            ..Default::default()
        };
        assert_eq!(f.run(7, "hello"), DropOutcome::UnknownPane);
        assert!(f.writes().is_empty());
    }

    /// AE5 — `claude` exited leaving a bash prompt in the *same* pane, so the
    /// pane id still matches. Without this guard the paste plus Enter executes
    /// the caption as a shell command.
    #[test]
    fn a_pane_whose_foreground_is_not_an_agent_is_refused_with_no_write() {
        let f = FakePane {
            resolved: Some(7),
            agent_probes: std::cell::RefCell::new(vec![false]),
            ..Default::default()
        };
        assert_eq!(f.run(7, "hello"), DropOutcome::NotAgent);
        assert!(f.writes().is_empty(), "nothing typed into the shell");
    }

    /// The residual-race mitigation (KTD6): the guards are check-then-act, and
    /// delivery is two writes 150ms apart. If `claude` exits in that window the
    /// re-probe catches it and the Enter is abandoned — leaving unsubmitted
    /// text in a shell rather than an executed command.
    #[test]
    fn an_exit_between_paste_and_enter_abandons_the_submit() {
        let f = FakePane {
            resolved: Some(7),
            // agent at the first probe, gone by the pre-Enter re-probe
            agent_probes: std::cell::RefCell::new(vec![true, false]),
            ..Default::default()
        };
        let out = f.run(7, "hello");
        assert!(
            matches!(out, DropOutcome::SubmitIncomplete(_)),
            "got {out:?}"
        );
        let w = f.writes();
        assert_eq!(w.len(), 1, "the paste landed, the Enter did not");
        assert_ne!(w[0], crate::feed::io::SUBMIT);
    }

    /// AE9 first half: the paste failed, so nothing reached the pane and the
    /// caller may unlink.
    #[test]
    fn a_failed_paste_reports_paste_failed_and_never_attempts_the_enter() {
        let f = FakePane {
            resolved: Some(7),
            agent_probes: std::cell::RefCell::new(vec![true]),
            fail_write: Some(0),
            ..Default::default()
        };
        let out = f.run(7, "hello");
        assert!(matches!(out, DropOutcome::PasteFailed(_)), "got {out:?}");
        assert!(f.writes().is_empty());
    }

    /// AE9 second half: the paste landed but the Enter did not. The composed
    /// prompt is sitting in the composer, so the caller must **keep** the image
    /// — unlinking would strand a path the user is about to hit Enter on.
    #[test]
    fn a_failed_enter_reports_submit_incomplete_so_the_image_is_kept() {
        let f = FakePane {
            resolved: Some(7),
            agent_probes: std::cell::RefCell::new(vec![true]),
            fail_write: Some(1),
            ..Default::default()
        };
        let out = f.run(7, "hello");
        assert!(
            matches!(out, DropOutcome::SubmitIncomplete(_)),
            "got {out:?}"
        );
        assert_eq!(f.writes().len(), 1, "the paste is on screen");
    }

    /// The paste goes through `paste_payload`, so a caption that tries to forge
    /// the bracketed-paste end marker cannot.
    #[test]
    fn the_pasted_payload_is_bracketed_and_strips_escapes() {
        let f = FakePane::agent(7);
        f.run(7, "cap\x1b[201~tion");
        let first = f.writes().remove(0);
        let s = String::from_utf8_lossy(&first).into_owned();
        assert!(s.starts_with("\x1b[200~"), "{s:?}");
        assert!(s.ends_with("\x1b[201~"), "{s:?}");
        assert_eq!(s.matches("\x1b[201~").count(), 1, "no forged end marker");
    }

    // ---- test readers ----

    /// Counts how many bytes were actually pulled, so the oversize path can
    /// assert it stopped early instead of draining the whole body.
    struct CountingReader<'a> {
        inner: &'a [u8],
        read: usize,
    }

    impl Read for CountingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    /// Returns at most one byte per call — the shape a slow socket presents.
    struct DribbleReader<'a> {
        inner: &'a [u8],
    }

    impl Read for DribbleReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner.read(&mut buf[..1])
        }
    }
}
