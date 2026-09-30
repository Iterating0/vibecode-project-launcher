use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// Allocates ports only while a project is being started. A successful probe
/// closes its sockets immediately and keeps a logical reservation until the
/// process manager releases it.
#[derive(Debug, Default)]
pub struct PortManager {
    reserved: Mutex<HashSet<u16>>,
}

impl PortManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reserve(&self, preferred: u16) -> Result<u16, String> {
        if preferred == 0 {
            return Err("preferred port must be greater than zero".to_owned());
        }

        let mut reserved = self
            .reserved
            .lock()
            .map_err(|_| "port reservation lock is poisoned".to_owned())?;

        // Walk the full non-zero u16 range, wrapping after 65535 so a busy
        // preferred port can still choose a low available port.
        for offset in 0..u32::from(u16::MAX) {
            let candidate = candidate_after(preferred, offset);
            if reserved.contains(&candidate) {
                continue;
            }

            let available = probe_port(candidate)
                .map_err(|error| format!("failed to probe port {candidate}: {error}"))?;
            if available {
                reserved.insert(candidate);
                return Ok(candidate);
            }
        }

        Err("no available TCP port could be reserved".to_owned())
    }

    pub fn release(&self, port: u16) {
        if let Ok(mut reserved) = self.reserved.lock() {
            reserved.remove(&port);
        }
    }
}

fn candidate_after(preferred: u16, offset: u32) -> u16 {
    let zero_based = (u32::from(preferred) - 1 + offset) % u32::from(u16::MAX);
    (zero_based + 1) as u16
}

fn probe_port(port: u16) -> io::Result<bool> {
    // A wildcard bind catches listeners bound to a specific local interface
    // and keeps the check independent of the eventual dev server framework.
    match probe_socket(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        false,
    ) {
        Ok(()) => {}
        Err(error) if is_family_unavailable(&error) => {}
        Err(error) if is_port_unavailable(&error) => return Ok(false),
        Err(error) => return Err(error),
    }

    // Probe IPv6 separately. `only_v6` avoids a dual-stack IPv6 wildcard
    // binding being mistaken for a collision with the IPv4 probe.
    match probe_socket(
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        true,
    ) {
        Ok(()) => Ok(true),
        Err(error) if is_family_unavailable(&error) => Ok(true),
        Err(error) if is_port_unavailable(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn probe_socket(address: SocketAddr, only_v6: bool) -> io::Result<()> {
    let (domain, protocol) = match address {
        SocketAddr::V4(_) => (Domain::IPV4, Protocol::TCP),
        SocketAddr::V6(_) => (Domain::IPV6, Protocol::TCP),
    };
    let socket = Socket::new(domain, Type::STREAM, Some(protocol))?;

    #[cfg(windows)]
    set_exclusive_addr_use(&socket)?;

    if only_v6 {
        socket.set_only_v6(true)?;
    }

    socket.bind(&SockAddr::from(address))
}

fn is_family_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    )
}

fn is_port_unavailable(error: &io::Error) -> bool {
    // Windows may report WSAEACCES (10013) rather than WSAEADDRINUSE when
    // SO_EXCLUSIVEADDRUSE is set and another listener already owns the port.
    // Treat both as a busy candidate and continue allocation.
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

#[cfg(windows)]
fn set_exclusive_addr_use(socket: &Socket) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, SOCKET_ERROR, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
    };

    let enabled: i32 = 1;
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&enabled as *const i32).cast(),
            size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_not_a_valid_preferred_port() {
        let manager = PortManager::new();
        let error = manager.reserve(0).unwrap_err();
        assert!(error.contains("greater than zero"));
    }

    #[test]
    fn reservations_are_unique_until_released() {
        let manager = PortManager::new();
        let first = manager.reserve(40_000).unwrap();
        let second = manager.reserve(first).unwrap();
        assert_ne!(first, second);

        manager.release(first);
        manager.release(second);
    }

    #[test]
    fn reservations_can_be_released_and_allocated_again() {
        let manager = PortManager::new();
        let first = manager.reserve(40_100).unwrap();
        manager.release(first);
        let second = manager.reserve(first).unwrap();
        assert_eq!(second, first);
        manager.release(second);
    }

    #[test]
    fn concurrent_reservations_do_not_duplicate_ports() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(PortManager::new());
        let workers = (0..4)
            .map(|_| {
                let manager = Arc::clone(&manager);
                thread::spawn(move || manager.reserve(40_200).unwrap())
            })
            .collect::<Vec<_>>();

        let ports = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let unique = ports.iter().copied().collect::<HashSet<_>>();
        assert_eq!(unique.len(), ports.len());

        for port in ports {
            manager.release(port);
        }
    }

    #[test]
    fn an_existing_listener_is_skipped() {
        use std::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let occupied = listener.local_addr().unwrap().port();
        let manager = PortManager::new();

        let reserved = manager.reserve(occupied).unwrap();
        assert_ne!(reserved, occupied);
        manager.release(reserved);
        drop(listener);
    }
}
