use crate::{Error, checksum::ChecksumUpdate, ref_mut_at::ref_mut_at};
use aya_ebpf::programs::XdpContext;
use aya_log_ebpf::debug;
use network_types::{eth::EthHdr, udp::UdpHdr};

/// Represents a UDP header within our packet.
pub struct Udp<'a> {
    inner: &'a mut UdpHdr,
    ctx: &'a XdpContext,
}

impl<'a> Udp<'a> {
    /// # SAFETY
    ///
    /// You must not create multiple [`Udp`] structs at same time.
    #[inline(always)]
    pub unsafe fn parse(ctx: &'a XdpContext, ip_header_length: usize) -> Result<Self, Error> {
        Ok(Self {
            ctx,
            // Safety: We are forwarding the constraint.
            inner: unsafe { ref_mut_at::<UdpHdr>(ctx, EthHdr::LEN + ip_header_length) }?,
        })
    }

    pub fn src(&self) -> u16 {
        u16::from_be_bytes(self.inner.source)
    }

    pub fn dst(&self) -> u16 {
        u16::from_be_bytes(self.inner.dest)
    }

    pub fn len(&self) -> u16 {
        u16::from_be_bytes(self.inner.len)
    }

    pub fn payload_len(&self) -> u16 {
        self.len() - UdpHdr::LEN as u16
    }

    /// Update this packet with a new source, destination, and length.
    #[inline(always)]
    pub fn update(
        self,
        ip_pseudo_header: ChecksumUpdate,
        new_src: u16,
        new_dst: u16,
        new_len: u16,
        channel_number: u16,
        channel_data_len: u16,
    ) {
        let src = self.src();
        let dst = self.dst();
        let len = self.len();

        let payload_checksum_update = if new_len > len {
            ChecksumUpdate::default()
                .add_u16(channel_number)
                .add_u16(channel_data_len)
        } else {
            ChecksumUpdate::default()
                .remove_u16(channel_number)
                .remove_u16(channel_data_len)
        };

        self.inner.source = new_src.to_be_bytes();
        self.inner.dest = new_dst.to_be_bytes();
        self.inner.len = new_len.to_be_bytes();

        let ip_pseudo_header = ip_pseudo_header.remove_u16(len).add_u16(new_len);

        if crate::config::udp_checksum_enabled() {
            self.inner.check = ChecksumUpdate::new(u16::from_be_bytes(self.inner.check))
                .add_update(ip_pseudo_header)
                .add_update(payload_checksum_update)
                .remove_u16(len)
                .add_u16(new_len)
                .remove_u16(src)
                .add_u16(new_src)
                .remove_u16(dst)
                .add_u16(new_dst)
                .into_checksum()
                .to_be_bytes()
        } else {
            self.inner.check = [0, 0];
        }

        debug!(
            self.ctx,
            "UDP header update: src {} -> {}; dst {} -> {}; len {} -> {}",
            src,
            new_src,
            dst,
            new_dst,
            len,
            new_len,
        );
    }
}
