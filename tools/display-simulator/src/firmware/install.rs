//! Acting on one offer's download ("Checks before installing", steps 1 to 9), as a machine the body
//! is pushed into a piece at a time. The network driver and the conformance vectors both use it, so
//! they can't disagree.
//!
//! Steps 1 to 7 are `firmware_package::Display`'s. Nothing is written until they have all passed;
//! then the image is streamed into the spare slot and hashed, and exactly `image_size` bytes are
//! read, never more.

use firmware_package::limits::HEADER_LEN;
use firmware_package::{Display, DisplayHeader, Manifest, Refusal};
use hub_protocol::firmware::{ReportReason, ReportState};
use ring::digest::{Context, SHA256};

/// Why a write to the spare slot stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlashError {
    /// A write, or the check at the end, failed (`flash`).
    Failed,
    /// The power went off (a test switch). Nothing else happens until the next start.
    PowerCut,
}

/// The spare app slot, as ESP-IDF's `esp_ota_*` functions see it.
pub trait Flash {
    /// `esp_ota_begin`: erase the slot for writing.
    fn begin(&mut self) -> Result<(), FlashError>;
    fn write(&mut self, bytes: &[u8]) -> Result<(), FlashError>;
    /// `esp_ota_abort`: give the slot up. It holds nothing bootable.
    fn abort(&mut self);
    /// `esp_ota_end`: finish, and check the image is a valid app image.
    fn end(&mut self) -> Result<(), FlashError>;
}

/// What went wrong after the checks. The old firmware is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Download,
    ImageHash,
    Flash,
}

impl Failure {
    pub fn reason(self) -> ReportReason {
        match self {
            Self::Download => ReportReason::Download,
            Self::ImageHash => ReportReason::ImageHash,
            Self::Flash => ReportReason::Flash,
        }
    }
}

/// How an attempt ended, short of an image ready to boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Nothing was written.
    Refused(Refusal),
    Failed(Failure),
    /// The power went off part-way (a test switch).
    PowerCut,
}

impl Ended {
    /// The final report's state and reason, for an attempt that ended this way.
    pub fn report(self) -> Option<(ReportState, ReportReason)> {
        match self {
            Self::Refused(r) => Some((ReportState::Refused, refusal_reason(r))),
            Self::Failed(f) => Some((ReportState::Failed, f.reason())),
            Self::PowerCut => None,
        }
    }
}

pub fn refusal_reason(r: Refusal) -> ReportReason {
    match r {
        Refusal::NoTrustedKeys => ReportReason::NoTrustedKeys,
        Refusal::TooLarge => ReportReason::TooLarge,
        Refusal::BadFormat => ReportReason::BadFormat,
        Refusal::UntrustedKey => ReportReason::UntrustedKey,
        Refusal::BadSignature => ReportReason::BadSignature,
        Refusal::WrongDeviceType => ReportReason::WrongDeviceType,
        Refusal::WrongHardware => ReportReason::WrongHardware,
        Refusal::Downgrade => ReportReason::Downgrade,
    }
}

/// What a push achieved. `Complete` is large next to the others, but comes once per download, so
/// the manifest isn't boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Keep reading.
    More,
    /// The manifest passed every check, and the spare slot is open: report `downloading`.
    Checked,
    /// The image is written, its hash matches and the slot is finished. Set it to boot and restart.
    Complete(Manifest),
}

/// One per download, so the large variants aren't boxed.
#[allow(clippy::large_enum_variant)]
enum Phase {
    Header(Vec<u8>),
    Manifest {
        header: DisplayHeader,
        buf: Vec<u8>,
    },
    Image {
        manifest: Manifest,
        left: u64,
        hash: Context,
    },
    Done,
}

pub struct Installer<'d> {
    display: Display<'d>,
    content_length: u64,
    phase: Phase,
    read: u64,
}

impl<'d> Installer<'d> {
    /// Steps 1 and 2 as far as they can go before the download is requested: no keys, or an offer
    /// larger than a header, the largest manifest and the spare slot.
    pub fn before_request(display: &Display<'_>, offer_size: u64) -> Result<(), Refusal> {
        display.check_offer(offer_size, offer_size)
    }

    /// Step 2 with the response: its `Content-Length` must equal the offer's `size`. A response
    /// without one is refused the same way, without reading its body.
    pub fn start(
        display: Display<'d>,
        offer_size: u64,
        content_length: Option<u64>,
    ) -> Result<Self, Refusal> {
        // No length at all can't equal the offer's.
        display.check_offer(offer_size, content_length.unwrap_or(u64::MAX))?;
        let content_length = content_length.expect("checked equal to the offer's size");
        Ok(Self {
            display,
            content_length,
            phase: Phase::Header(Vec::with_capacity(HEADER_LEN)),
            read: 0,
        })
    }

    /// Bytes of the body taken so far.
    pub fn bytes_read(&self) -> u64 {
        self.read
    }

    /// Whether the spare slot has been opened for writing.
    pub fn writing(&self) -> bool {
        matches!(self.phase, Phase::Image { .. })
    }

    /// Takes as much of `chunk` as the current step needs, and says how many bytes it took. Call it
    /// again with the rest until the chunk is used up or the image is complete; bytes after the
    /// image are never taken.
    pub fn push(&mut self, chunk: &[u8], flash: &mut dyn Flash) -> Result<(usize, Progress), Ended> {
        match &mut self.phase {
            Phase::Header(buf) => {
                let take = (HEADER_LEN - buf.len()).min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
                self.read += take as u64;
                if buf.len() < HEADER_LEN {
                    return Ok((take, Progress::More));
                }
                let bytes: [u8; HEADER_LEN] = buf.as_slice().try_into().expect("a whole header");
                // 3 and 4.
                let header = self.display.check_header(&bytes).map_err(Ended::Refused)?;
                self.phase = Phase::Manifest {
                    buf: Vec::with_capacity(header.header.manifest_len as usize),
                    header,
                };
                Ok((take, Progress::More))
            }
            Phase::Manifest { header, buf } => {
                let want = header.header.manifest_len as usize;
                let take = (want - buf.len()).min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
                self.read += take as u64;
                if buf.len() < want {
                    return Ok((take, Progress::More));
                }
                // 5, 6 and 7.
                let manifest = self
                    .display
                    .check_manifest(header, buf, self.content_length)
                    .map_err(Ended::Refused)?;
                // 8. Only now is the spare slot opened.
                match flash.begin() {
                    Ok(()) => {}
                    Err(FlashError::PowerCut) => return Err(Ended::PowerCut),
                    Err(FlashError::Failed) => {
                        flash.abort();
                        self.phase = Phase::Done;
                        return Err(Ended::Failed(Failure::Flash));
                    }
                }
                self.phase = Phase::Image {
                    left: manifest.image_size,
                    manifest,
                    hash: Context::new(&SHA256),
                };
                Ok((take, Progress::Checked))
            }
            Phase::Image { manifest, left, hash } => {
                let take = (*left).min(chunk.len() as u64) as usize;
                let piece = &chunk[..take];
                match flash.write(piece) {
                    Ok(()) => {}
                    Err(FlashError::PowerCut) => return Err(Ended::PowerCut),
                    Err(FlashError::Failed) => {
                        flash.abort();
                        self.phase = Phase::Done;
                        return Err(Ended::Failed(Failure::Flash));
                    }
                }
                hash.update(piece);
                self.read += take as u64;
                *left -= take as u64;
                if *left > 0 {
                    return Ok((take, Progress::More));
                }
                let manifest = manifest.clone();
                let digest = std::mem::replace(hash, Context::new(&SHA256)).finish();
                self.phase = Phase::Done;
                if digest.as_ref() != manifest.image_sha256 {
                    flash.abort();
                    return Err(Ended::Failed(Failure::ImageHash));
                }
                // 9.
                match flash.end() {
                    Ok(()) => Ok((take, Progress::Complete(manifest))),
                    Err(FlashError::PowerCut) => Err(Ended::PowerCut),
                    Err(FlashError::Failed) => {
                        flash.abort();
                        Err(Ended::Failed(Failure::Flash))
                    }
                }
            }
            Phase::Done => Ok((0, Progress::More)),
        }
    }

    /// The body ended, sent nothing for too long, or took too long in all, before the image was
    /// complete (`download`). A slot being written is given up.
    pub fn give_up(&mut self, flash: &mut dyn Flash) -> Ended {
        if self.writing() {
            flash.abort();
        }
        self.phase = Phase::Done;
        Ended::Failed(Failure::Download)
    }
}

/// What a spare slot in memory saw: for the conformance vectors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryFlash {
    pub begun: bool,
    pub aborted: bool,
    pub ended: bool,
    pub written: Vec<u8>,
}

impl Flash for MemoryFlash {
    fn begin(&mut self) -> Result<(), FlashError> {
        self.begun = true;
        self.written.clear();
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), FlashError> {
        self.written.extend_from_slice(bytes);
        Ok(())
    }

    fn abort(&mut self) {
        self.aborted = true;
        self.written.clear();
    }

    fn end(&mut self) -> Result<(), FlashError> {
        self.ended = true;
        Ok(())
    }
}

/// How a body given in memory finishes, after its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterBody {
    /// The connection closes.
    Ends,
    /// The connection stays open and sends nothing more.
    Stalls,
}

/// What became of one attempt on a body in memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempted {
    pub result: Result<Manifest, Ended>,
    /// The download was requested (steps 1 and 2 passed as far as they can before it).
    pub requested: bool,
    /// `downloading` was reported.
    pub checked: bool,
    pub bytes_read: u64,
    pub flash: MemoryFlash,
}

/// One attempt with the response in memory: the reference for the conformance vectors.
pub fn attempt_in_memory(
    display: Display<'_>,
    offer_size: u64,
    content_length: Option<u64>,
    body: &[u8],
    after: AfterBody,
) -> Attempted {
    let mut flash = MemoryFlash::default();
    let mut out = Attempted {
        result: Err(Ended::Refused(Refusal::NoTrustedKeys)),
        requested: false,
        checked: false,
        bytes_read: 0,
        flash: MemoryFlash::default(),
    };
    if let Err(r) = Installer::before_request(&display, offer_size) {
        out.result = Err(Ended::Refused(r));
        return out;
    }
    out.requested = true;
    let mut installer = match Installer::start(display, offer_size, content_length) {
        Ok(i) => i,
        Err(r) => {
            out.result = Err(Ended::Refused(r));
            return out;
        }
    };
    // The body arrives in small pieces, so every boundary inside a step is crossed.
    const PIECE: usize = 97;
    let mut rest = body;
    let result = 'read: loop {
        if rest.is_empty() {
            // Whether it ends or stalls, nothing more comes: the display gives up either way.
            let _ = after;
            break 'read Err(installer.give_up(&mut flash));
        }
        let mut piece = &rest[..rest.len().min(PIECE)];
        rest = &rest[piece.len()..];
        while !piece.is_empty() {
            match installer.push(piece, &mut flash) {
                Ok((n, progress)) => {
                    piece = &piece[n..];
                    match progress {
                        Progress::More => {}
                        Progress::Checked => out.checked = true,
                        Progress::Complete(m) => break 'read Ok(m),
                    }
                }
                Err(e) => break 'read Err(e),
            }
        }
    };
    out.result = result;
    out.bytes_read = installer.bytes_read();
    out.flash = flash;
    out
}
