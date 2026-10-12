use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};

pub const VERSION: u32 = 2;
pub const SOCKET: &str = "/run/omash-tun/control.sock";
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
// What a helper accepts when its status names no `max_frame`.
const LEGACY_MAX_FRAME: usize = 4 * 1024 * 1024;
pub const RPC_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
// Covers a full validation/apply transaction while still bounding a stalled owner.
pub const LEASE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub id: String,
    pub action: Action,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Hello,
    Status,
    Start { yaml: String, validate_only: bool },
    Apply { yaml: String, generation: u64 },
    Stop { generation: u64 },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub protocol: u32,
    pub helper_version: String,
    pub uid: u32,
    pub data_dir: PathBuf,
    pub running: bool,
    pub tun_active: bool,
    pub pid: Option<u32>,
    pub generation: u64,
    pub revision: String,
    pub device: Option<String>,
    #[serde(default)]
    pub firewall_error: Option<String>,
    /// The largest frame this helper reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_frame: Option<usize>,
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub id: String,
    pub status: Status,
    pub error: Option<String>,
}

pub fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The other side speaks another protocol version; `peer` is the version it announced.
/// Callers classify it with `downcast_ref`, which keeps working under `.context(..)` layers.
#[derive(Debug)]
pub struct ProtocolMismatch {
    pub peer: u32,
}
impl fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TUN helper protocol mismatch; run `omash tun setup` to update it")
    }
}
impl std::error::Error for ProtocolMismatch {}

pub fn check_version(version: u32) -> Result<()> {
    if version != VERSION {
        return Err(ProtocolMismatch { peer: version }.into());
    }
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let size = stream.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        bail!("invalid TUN frame size");
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid TUN protocol frame")
}
pub async fn write_frame<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    write_frame_within(stream, value, MAX_FRAME).await
}
async fn write_frame_within<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
    limit: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        bail!("TUN request exceeds {} MiB", MAX_FRAME >> 20);
    }
    // Only a request that an updated helper would accept is worth updating it for.
    if bytes.len() > limit {
        bail!(
            "TUN request exceeds the {} MiB the installed TUN helper accepts; run `omash tun setup` to update it",
            limit >> 20
        );
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Marks every `Client::connect` failure. Callers classify with `downcast_ref`,
/// which keeps working after further `.context(..)` layers are added.
#[derive(Debug)]
pub struct HelperUnavailable;
impl fmt::Display for HelperUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TUN helper unavailable; run `omash tun setup`")
    }
}
impl std::error::Error for HelperUnavailable {}

pub struct Client {
    stream: UnixStream,
    pub status: Status,
}
impl Client {
    pub async fn connect() -> Result<Self> {
        Self::connect_at(Path::new(SOCKET)).await
    }
    async fn connect_at(socket: &Path) -> Result<Self> {
        Self::open(socket).await.context(HelperUnavailable)
    }
    /// The helper's status, also when it serves another data directory: uninstall reads from
    /// it whether exceptions are left that the helper could not remove.
    pub async fn helper_status() -> Result<Status> {
        Self::helper_status_at(Path::new(SOCKET)).await
    }
    async fn helper_status_at(socket: &Path) -> Result<Status> {
        Self::hello(socket)
            .await
            .map(|client| client.status)
            .context(HelperUnavailable)
    }
    async fn open(socket: &Path) -> Result<Self> {
        let client = Self::hello(socket).await?;
        let uid = unsafe { libc::geteuid() };
        if client.status.uid != uid || client.status.data_dir != crate::config::Config::data_dir() {
            bail!("TUN helper installation user/data directory differs; run `omash tun setup`");
        }
        Ok(client)
    }
    /// Connects to this user's helper and checks that it speaks this protocol.
    async fn hello(socket: &Path) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket))
            .await
            .with_context(|| {
                format!(
                    "connecting to {} timed out after {}s",
                    socket.display(),
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        let peer = stream.peer_cred()?;
        let uid = unsafe { libc::geteuid() };
        if peer.uid() != uid {
            bail!("TUN helper belongs to another user");
        }
        let mut client = Self {
            stream,
            status: Status::default(),
        };
        client.request(Action::Hello).await?;
        check_version(client.status.protocol)?;
        Ok(client)
    }
    #[cfg(test)]
    pub(crate) fn from_parts(stream: UnixStream, status: Status) -> Self {
        Self { stream, status }
    }
    pub async fn request(&mut self, action: Action) -> Result<Status> {
        let request = Request {
            version: VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            action,
        };
        // An older helper drops a connection whose frame exceeds its limit, and a dropped
        // owner connection stops the running core: refuse such a request here instead.
        let limit = self
            .status
            .max_frame
            .unwrap_or(LEGACY_MAX_FRAME)
            .min(MAX_FRAME);
        let reply: Reply = tokio::time::timeout(RPC_TIMEOUT, async {
            write_frame_within(&mut self.stream, &request, limit).await?;
            read_frame(&mut self.stream).await
        })
        .await
        .context("TUN helper request timed out")??;
        check_version(reply.version)?;
        if reply.id != request.id {
            bail!("TUN reply request ID mismatch");
        }
        self.status = reply.status;
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        Ok(self.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn helper_rejects_oversized_frame() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_be_bytes().as_slice().to_owned();
        // The size alone is refused: the missing payload must not be what fails.
        let Err(error) = read_frame::<Request>(&mut bytes.as_slice()).await else {
            panic!("an oversized frame was accepted");
        };
        assert_eq!(error.to_string(), "invalid TUN frame size");
        bytes.clear();
    }
    #[tokio::test]
    async fn frames_larger_than_4_mib_round_trip() {
        let yaml = "x".repeat(5 << 20);
        let mut bytes = Vec::new();
        let action = Action::Start {
            yaml: yaml.clone(),
            validate_only: false,
        };
        write_frame(&mut bytes, &action).await.unwrap();
        let read: Action = read_frame(&mut bytes.as_slice()).await.unwrap();
        assert!(matches!(read, Action::Start { yaml: y, .. } if y == yaml));
    }
    #[tokio::test]
    async fn requests_above_16_mib_are_refused_whatever_the_helper_accepts() {
        let action = Action::Apply {
            yaml: "x".repeat(17 << 20),
            generation: 1,
        };
        // An updated helper would refuse it too, so `omash tun setup` is no advice here.
        for limit in [LEGACY_MAX_FRAME, MAX_FRAME] {
            let mut sink = Vec::new();
            let error = write_frame_within(&mut sink, &action, limit)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "TUN request exceeds 16 MiB");
            assert!(sink.is_empty());
        }
    }
    #[test]
    fn helper_rejects_protocol_mismatch() {
        assert!(check_version(VERSION + 1).is_err());
    }

    fn is_unavailable(error: &anyhow::Error) -> bool {
        error.downcast_ref::<HelperUnavailable>().is_some()
    }

    // Answers the Hello of one connection with `status`, then holds it open.
    fn fake_helper(socket: &Path, status: Status) {
        fake_helper_of(socket, VERSION, status);
    }
    // The same, for a helper that speaks protocol `version`.
    fn fake_helper_of(socket: &Path, version: u32, status: Status) {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Request = read_frame(&mut stream).await.unwrap();
            let reply = Reply {
                version,
                id: request.id,
                status,
                error: None,
            };
            write_frame(&mut stream, &reply).await.unwrap();
            let _ = read_frame::<Request>(&mut stream).await;
        });
    }
    fn good_status() -> Status {
        Status {
            protocol: VERSION,
            uid: unsafe { libc::geteuid() },
            data_dir: crate::config::Config::data_dir(),
            ..Default::default()
        }
    }

    #[test]
    fn helper_unavailable_survives_extra_context() {
        let error = Err::<(), _>(anyhow::anyhow!("socket is gone"))
            .context(HelperUnavailable)
            .context("new configuration failed; no previous core to restore")
            .with_context(|| format!("{} failed", "routing-mode rollback"))
            .unwrap_err();
        assert!(is_unavailable(&error));
        let text = format!("{error:#}");
        assert!(text.contains("TUN helper unavailable; run `omash tun setup`: socket is gone"));
        assert!(!is_unavailable(&anyhow::anyhow!(
            "TUN helper unavailable; run `omash tun setup`"
        )));
    }

    #[tokio::test]
    async fn missing_socket_is_helper_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let error = Client::connect_at(&dir.path().join("s.sock"))
            .await
            .err()
            .unwrap();
        assert!(is_unavailable(&error));
        let text = format!("{error:#}");
        assert!(
            text.starts_with(
                "TUN helper unavailable; run `omash tun setup`: failed to connect to "
            ),
            "{text}"
        );
        assert!(text.contains("No such file or directory"), "{text}");
    }

    #[tokio::test]
    async fn dropped_connection_is_helper_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let error = Client::connect_at(&socket).await.err().unwrap();
        assert!(is_unavailable(&error), "{error:#}");
    }

    #[tokio::test]
    async fn protocol_and_installation_mismatches_are_helper_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                Status {
                    protocol: VERSION + 1,
                    ..good_status()
                },
                "protocol mismatch",
            ),
            (
                Status {
                    uid: good_status().uid + 1,
                    ..good_status()
                },
                "user/data directory differs",
            ),
            (
                Status {
                    data_dir: "/nonexistent/omash".into(),
                    ..good_status()
                },
                "user/data directory differs",
            ),
        ];
        for (index, (status, expected)) in cases.into_iter().enumerate() {
            let socket = dir.path().join(format!("{index}.sock"));
            fake_helper(&socket, status);
            let error = Client::connect_at(&socket).await.err().unwrap();
            assert!(is_unavailable(&error), "{error:#}");
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    #[tokio::test]
    async fn matching_helper_connects() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        fake_helper(&socket, good_status());
        let client = Client::connect_at(&socket).await.unwrap();
        assert_eq!(client.status.protocol, VERSION);
    }

    #[tokio::test]
    async fn an_older_helper_reports_its_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        fake_helper_of(
            &socket,
            1,
            Status {
                protocol: 1,
                ..good_status()
            },
        );
        let error = Client::connect_at(&socket).await.err().unwrap();
        assert!(is_unavailable(&error), "{error:#}");
        let mismatch = error.downcast_ref::<ProtocolMismatch>();
        assert_eq!(mismatch.map(|m| m.peer), Some(1), "{error:#}");
        assert!(
            format!("{error:#}").contains("protocol mismatch"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn helper_status_ignores_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let status = Status {
            data_dir: "/nonexistent/omash".into(),
            firewall_error: Some("nft failed".into()),
            ..good_status()
        };
        let socket = dir.path().join("connect.sock");
        fake_helper(&socket, status.clone());
        assert!(Client::connect_at(&socket).await.is_err());
        let socket = dir.path().join("status.sock");
        fake_helper(&socket, status);
        let read = Client::helper_status_at(&socket).await.unwrap();
        assert_eq!(read.firewall_error.as_deref(), Some("nft failed"));
    }

    // Answers every request with `status` and, once the connection ends, reports the
    // requests it read.
    fn recording_helper(
        socket: &Path,
        status: Status,
    ) -> tokio::sync::oneshot::Receiver<Vec<Request>> {
        let listener = UnixListener::bind(socket).unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut requests = Vec::new();
            while let Ok(request) = read_frame::<Request>(&mut stream).await {
                let reply = Reply {
                    version: VERSION,
                    id: request.id.clone(),
                    status: status.clone(),
                    error: None,
                };
                write_frame(&mut stream, &reply).await.unwrap();
                requests.push(request);
            }
            let _ = sender.send(requests);
        });
        receiver
    }

    #[tokio::test]
    async fn an_older_helper_never_receives_a_frame_above_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        // Helpers from before the 16 MiB limit send no `max_frame`: they drop the connection on
        // a larger frame, and a dropped owner connection stops the running core.
        let status = good_status();
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("max_frame")
        );
        let received = recording_helper(&socket, status);
        let mut client = Client::open(&socket).await.unwrap();
        let apply = Action::Apply {
            yaml: "x".repeat(5 << 20),
            generation: 1,
        };
        let error = client.request(apply).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("omash tun setup"),
            "{error:#}"
        );
        // Not one byte of it was sent: the next request on the connection arrives intact.
        tokio::time::timeout(Duration::from_secs(5), client.request(Action::Status))
            .await
            .expect("the connection carries part of the refused frame")
            .unwrap();
        drop(client);
        let requests = received.await.unwrap();
        let actions: Vec<_> = requests.iter().map(|r| &r.action).collect();
        assert!(matches!(actions[..], [Action::Hello, Action::Status]));
    }

    #[tokio::test]
    async fn a_helper_advertising_16_mib_receives_a_5_mib_apply() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let status = Status {
            max_frame: Some(16 * 1024 * 1024),
            ..good_status()
        };
        let received = recording_helper(&socket, status);
        let mut client = Client::open(&socket).await.unwrap();
        let yaml = "x".repeat(5 << 20);
        let apply = Action::Apply {
            yaml: yaml.clone(),
            generation: 1,
        };
        client.request(apply).await.unwrap();
        drop(client);
        let requests = received.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(&requests[1].action, Action::Apply { yaml: y, .. } if *y == yaml));
    }
}
