use std::{fs::File, io::Read, path::Path};

use object::Object;

use crate::Error;

mod elf;
mod thumb;

/// A supported combination of Luna wire layouts and handshake behavior.
///
/// Each variant names a distinguishing wire feature. The same profile can
/// appear in several webOS releases.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ProtocolProfile {
    /// Local registration returns a listener descriptor and uses a 12-byte header.
    LocalListener,
    /// A separate initiator message delivers each incoming peer descriptor.
    InitiatorInfo,
    /// Replies carry a MIME type, and peers connect without `ClientConnected`.
    TypedReplies,
    /// Registration uses `NodeUp`, and peers acknowledge with `ClientConnected`.
    NodeUpWithClientConnected,
    /// Registration puts trust rules before the flags.
    TrustBeforeFlags,
    /// Unified peer offers and wire messages for permanent subscriptions.
    PermanentSubscriptions,
    /// The permanent subscription transport with native intent message types.
    IntentResolution,
    /// Replies carry an explicit payload length and alignment padding.
    LengthPrefixedReplies,
}

impl ProtocolProfile {
    pub(crate) const DEFAULT: Self = Self::LengthPrefixedReplies;

    pub(crate) fn hub_path(self) -> &'static Path {
        Path::new(if self == Self::LocalListener {
            "/tmp/com.palm.private_hub"
        } else {
            "/var/run/luna-service2/com.palm.hub"
        })
    }

    pub(crate) fn detect() -> Result<Self, Error> {
        detect_in(Path::new("/"))
    }

    pub(crate) fn sends_node_up(self) -> bool {
        matches!(
            self,
            Self::LocalListener
                | Self::InitiatorInfo
                | Self::TypedReplies
                | Self::NodeUpWithClientConnected
                | Self::TrustBeforeFlags
        )
    }

    pub(crate) fn sends_client_connected(self) -> bool {
        !matches!(
            self,
            Self::LocalListener | Self::InitiatorInfo | Self::TypedReplies
        )
    }

    pub(crate) fn reply_layout(self) -> ReplyLayout {
        match self {
            Self::LocalListener | Self::InitiatorInfo => ReplyLayout::Plain,
            Self::LengthPrefixedReplies => ReplyLayout::Sized,
            _ => ReplyLayout::Typed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyLayout {
    Plain,
    Typed,
    Sized,
}

// Read the installed binaries as ELF data. Some incompatible firmware builds have
// identical SONAMEs and version strings.
fn detect_in(root: &Path) -> Result<ProtocolProfile, Error> {
    let mut matched = None;
    for name in ["usr/lib/libluna-service2.so.3", "lib/libluna-service2.so.3"] {
        let Some(bytes) = read_elf(&root.join(name)) else {
            continue;
        };
        let Ok(elf) = object::File::parse(&*bytes) else {
            continue;
        };
        if elf.architecture() != object::Architecture::Arm || !elf.is_little_endian() {
            continue;
        }
        let Some(profile) = elf::profile_for_library(&elf) else {
            continue;
        };
        if matched.is_some_and(|other| other != profile) {
            return Err(Error::protocol(
                "installed Luna libraries indicate different wire profiles",
            ));
        }
        matched = Some(profile);
    }

    matched.ok_or_else(|| {
        Error::protocol(
            "unrecognized Luna firmware, select a protocol profile on the connection builder",
        )
    })
}

fn read_elf(path: &Path) -> Option<Vec<u8>> {
    const MAX_ELF_SIZE: usize = 16 * 1024 * 1024;
    use rustix::fs::{Mode, OFlags, open};

    let file = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?,
    );
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((MAX_ELF_SIZE + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_ELF_SIZE).then_some(bytes)
}
