//! The platform daemon as a library, so its pieces are testable: the unix
//! socket the harness reaches the API through, and the blob driver chosen
//! from configuration. `main.rs` is flags and wiring over this.

pub mod blobdriver;
/// Unix only: the socket is how a harness container reaches the API
/// (invariant 13), and harness runs are rootless Podman. A Windows build of
/// the daemon serves the port and refuses `--unix-socket`.
#[cfg(unix)]
pub mod unixsocket;

pub use blobdriver::{BlobConfig, blob_driver};
#[cfg(unix)]
pub use unixsocket::{SOCKET_MODE, SocketError, UnixSocket, unix_listener};
