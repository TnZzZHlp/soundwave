use std::{
    mem::{MaybeUninit, size_of},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use audio_stream_transport::PairingUri;
use thiserror::Error;
use windows::Win32::{
    Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR},
    NetworkManagement::{
        IpHelper::{
            GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK,
            IF_TYPE_TUNNEL, IP_ADAPTER_ADDRESSES_LH,
        },
        Ndis::{IfOperStatusUp, TUNNEL_TYPE_NONE},
    },
    Networking::WinSock::{AF_INET, SOCKADDR_IN, SOCKET_ADDRESS},
};

/// Connection data that is safe to put in a pairing QR code.
#[derive(Clone, Debug)]
pub(crate) struct PairingInfo {
    pub(crate) endpoint: SocketAddr,
    pub(crate) uri: String,
}

#[derive(Debug, Error)]
pub(crate) enum PairingError {
    #[error("QR pairing supports IPv4 only; bind to an IPv4 address or pass --pairing-host <IPv4>")]
    Ipv6Bind,
    #[error("{address} is not a usable LAN IPv4 address for QR pairing")]
    UnusableAddress { address: Ipv4Addr },
    #[error(
        "--pairing-host {pairing_host} does not match the non-wildcard --bind address {bind_host}"
    )]
    IncompatibleBind {
        pairing_host: Ipv4Addr,
        bind_host: Ipv4Addr,
    },
    #[error("could not enumerate Windows network adapters (Win32 error {status})")]
    AdapterEnumeration { status: u32 },
    #[error("could not find a usable LAN IPv4 address; pass --pairing-host <IPv4>")]
    NoCandidate,
    #[error(
        "multiple LAN IPv4 addresses are equally suitable ({addresses}); pass --pairing-host <IPv4>"
    )]
    AmbiguousCandidates { addresses: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdapterCandidate {
    address: Ipv4Addr,
    rank: u8,
}

/// Resolves the endpoint embedded in the QR code after QUIC has selected its
/// actual listening port. The advertised host can be overridden for VPN or
/// multi-adapter machines without changing the listening socket.
pub(crate) fn resolve_pairing_info(
    requested_bind: SocketAddr,
    actual_endpoint: SocketAddr,
    pairing_host: Option<Ipv4Addr>,
    fingerprint: [u8; 32],
) -> Result<PairingInfo, PairingError> {
    let host = match pairing_host {
        Some(host) => {
            validate_pairing_host(host)?;
            validate_bind_compatibility(requested_bind, host)?;
            host
        }
        None => match requested_bind.ip() {
            IpAddr::V4(host) if !host.is_unspecified() => {
                validate_pairing_host(host)?;
                host
            }
            IpAddr::V4(_) => select_automatic_pairing_host()?,
            IpAddr::V6(_) => return Err(PairingError::Ipv6Bind),
        },
    };
    let endpoint = SocketAddr::from((host, actual_endpoint.port()));

    Ok(PairingInfo {
        endpoint,
        uri: PairingUri::new(host, endpoint.port(), fingerprint).encode(),
    })
}

fn validate_bind_compatibility(
    requested_bind: SocketAddr,
    pairing_host: Ipv4Addr,
) -> Result<(), PairingError> {
    match requested_bind.ip() {
        IpAddr::V4(bind_host) if bind_host.is_unspecified() || bind_host == pairing_host => Ok(()),
        IpAddr::V4(bind_host) => Err(PairingError::IncompatibleBind {
            pairing_host,
            bind_host,
        }),
        IpAddr::V6(_) => Err(PairingError::Ipv6Bind),
    }
}

fn validate_pairing_host(address: Ipv4Addr) -> Result<(), PairingError> {
    if is_usable_pairing_address(address) {
        Ok(())
    } else {
        Err(PairingError::UnusableAddress { address })
    }
}

fn is_usable_pairing_address(address: Ipv4Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && !address.is_broadcast()
        && !address.is_link_local()
}

fn select_automatic_pairing_host() -> Result<Ipv4Addr, PairingError> {
    let mut candidates = enumerate_adapter_candidates()?;
    candidates.sort_unstable_by_key(|candidate| (candidate.rank, candidate.address));
    candidates.dedup_by_key(|candidate| candidate.address);

    let Some(best) = candidates.first().copied() else {
        return Err(PairingError::NoCandidate);
    };
    let equally_ranked = candidates
        .iter()
        .take_while(|candidate| candidate.rank == best.rank)
        .copied()
        .collect::<Vec<_>>();

    if equally_ranked.len() == 1 {
        return Ok(best.address);
    }

    Err(PairingError::AmbiguousCandidates {
        addresses: equally_ranked
            .iter()
            .map(|candidate| candidate.address.to_string())
            .collect::<Vec<_>>()
            .join(", "),
    })
}

fn enumerate_adapter_candidates() -> Result<Vec<AdapterCandidate>, PairingError> {
    let mut buffer_size = 0_u32;
    let status = unsafe {
        GetAdaptersAddresses(
            AF_INET.0 as u32,
            GAA_FLAG_INCLUDE_GATEWAYS,
            None,
            None,
            &mut buffer_size,
        )
    };
    if status == NO_ERROR.0 {
        return Ok(Vec::new());
    }
    if status != ERROR_BUFFER_OVERFLOW.0 {
        return Err(PairingError::AdapterEnumeration { status });
    }

    for _ in 0..3 {
        let entry_count = (buffer_size as usize).div_ceil(size_of::<IP_ADAPTER_ADDRESSES_LH>());
        let mut buffer: Vec<MaybeUninit<IP_ADAPTER_ADDRESSES_LH>> =
            Vec::with_capacity(entry_count.max(1));
        unsafe { buffer.set_len(entry_count.max(1)) };

        let mut returned_size = buffer_size;
        let status = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                None,
                Some(buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut returned_size,
            )
        };
        if status == NO_ERROR.0 {
            return Ok(collect_adapter_candidates(
                buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
            ));
        }
        if status != ERROR_BUFFER_OVERFLOW.0 {
            return Err(PairingError::AdapterEnumeration { status });
        }
        buffer_size = returned_size;
    }

    Err(PairingError::AdapterEnumeration {
        status: ERROR_BUFFER_OVERFLOW.0,
    })
}

fn collect_adapter_candidates(mut adapter: *mut IP_ADAPTER_ADDRESSES_LH) -> Vec<AdapterCandidate> {
    let mut candidates = Vec::new();

    while !adapter.is_null() {
        let adapter_ref = unsafe { &*adapter };
        let is_eligible = adapter_ref.OperStatus == IfOperStatusUp
            && adapter_ref.IfType != IF_TYPE_SOFTWARE_LOOPBACK
            && adapter_ref.IfType != IF_TYPE_TUNNEL
            && adapter_ref.TunnelType == TUNNEL_TYPE_NONE;
        if is_eligible {
            let rank = match (
                !adapter_ref.FirstGatewayAddress.is_null(),
                adapter_has_private_ipv4(adapter_ref.FirstUnicastAddress),
            ) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            };
            let mut unicast = adapter_ref.FirstUnicastAddress;
            while !unicast.is_null() {
                let unicast_ref = unsafe { &*unicast };
                if let Some(address) = ipv4_from_socket_address(&unicast_ref.Address)
                    && is_usable_pairing_address(address)
                {
                    candidates.push(AdapterCandidate { address, rank });
                }
                unicast = unicast_ref.Next;
            }
        }
        adapter = adapter_ref.Next;
    }

    candidates
}

fn adapter_has_private_ipv4(
    mut unicast: *mut windows::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH,
) -> bool {
    while !unicast.is_null() {
        let unicast_ref = unsafe { &*unicast };
        if let Some(address) = ipv4_from_socket_address(&unicast_ref.Address)
            && address.is_private()
            && is_usable_pairing_address(address)
        {
            return true;
        }
        unicast = unicast_ref.Next;
    }
    false
}

fn ipv4_from_socket_address(address: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
    if address.lpSockaddr.is_null() || address.iSockaddrLength < size_of::<SOCKADDR_IN>() as i32 {
        return None;
    }

    let sockaddr = unsafe { &*address.lpSockaddr.cast::<SOCKADDR_IN>() };
    if sockaddr.sin_family != AF_INET {
        return None;
    }
    let octets = unsafe { sockaddr.sin_addr.S_un.S_un_b };
    Some(Ipv4Addr::new(
        octets.s_b1,
        octets.s_b2,
        octets.s_b3,
        octets.s_b4,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_address_rejects_non_lan_endpoints() {
        for address in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::BROADCAST,
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::new(169, 254, 1, 1),
        ] {
            assert!(!is_usable_pairing_address(address));
        }
        assert!(is_usable_pairing_address(Ipv4Addr::new(192, 168, 1, 42)));
    }

    #[test]
    fn pairing_host_must_match_a_specific_bind() {
        let bind = SocketAddr::from(([192, 168, 1, 42], 48_400));
        assert!(validate_bind_compatibility(bind, Ipv4Addr::new(192, 168, 1, 42)).is_ok());
        assert!(validate_bind_compatibility(bind, Ipv4Addr::new(192, 168, 1, 43)).is_err());
    }
}
