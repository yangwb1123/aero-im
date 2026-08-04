use super::{KkFlag, KmMessage, KmMessageType, SrtCrypto, UnwrappedKmKeys};

#[derive(Debug)]
struct PendingRotation {
    target: KkFlag,
    request: KmMessage,
}

#[derive(Debug, Clone, Copy)]
struct PreviousKey {
    flag: KkFlag,
    switch_seq: u32,
}

/// Receive-side even/odd key lifecycle for one encrypted SRT session.
///
/// A KMREQ installs only the inactive slot. The first packet carrying that
/// slot promotes it; the previous slot is then accepted only for sequence
/// numbers before the observed switch boundary.
#[derive(Debug)]
pub(crate) struct SrtKeyRotation {
    passphrase: Option<Vec<u8>>,
    odd: Option<SrtCrypto>,
    active: KkFlag,
    pending: Option<PendingRotation>,
    previous: Option<PreviousKey>,
}

impl SrtKeyRotation {
    pub(crate) fn initial(passphrase: Option<Vec<u8>>) -> Self {
        Self {
            passphrase,
            odd: None,
            active: KkFlag::EvenKey,
            pending: None,
            previous: None,
        }
    }

    pub(crate) fn set_passphrase(&mut self, passphrase: Vec<u8>) {
        self.passphrase = Some(passphrase);
    }

    fn slot<'a>(&'a self, even: &'a Option<SrtCrypto>, flag: KkFlag) -> Option<&'a SrtCrypto> {
        match flag {
            KkFlag::EvenKey => even.as_ref(),
            KkFlag::OddKey => self.odd.as_ref(),
            KkFlag::Clear | KkFlag::Invalid => None,
        }
    }

    /// Validate and install a post-handshake KMREQ without switching early.
    pub(crate) fn install_request(
        &mut self,
        even: &mut Option<SrtCrypto>,
        km: &KmMessage,
    ) -> Result<(), String> {
        if km.msg_type != KmMessageType::Request || km.keki != 0 {
            return Err("post-handshake KMREQ must use request type and KEKI=0".into());
        }
        if let Some(pending) = &self.pending {
            return if pending.request == *km {
                Ok(())
            } else {
                Err("conflicting KMREQ received before the pending key switch".into())
            };
        }
        let passphrase = self.passphrase.as_deref().ok_or_else(|| {
            "post-handshake KMREQ received without a session passphrase".to_string()
        })?;
        let mut incoming = UnwrappedKmKeys::from_message(km, passphrase)
            .map_err(|error| format!("KMREQ key unwrap failed: {error}"))?;
        let target = opposite_key(self.active);
        let active = self
            .slot(even, self.active)
            .ok_or_else(|| "active SRT key slot is missing".to_string())?;
        if !km.key_flags.contains(target) {
            return Err("KMREQ does not carry the next alternating key slot".into());
        }
        if let Some(echoed_active) = incoming.get(self.active) {
            if echoed_active != active {
                return Err("dual-key KMREQ does not preserve the active SEK".into());
            }
        }
        let target_key = incoming
            .take(target)
            .ok_or_else(|| "KMREQ target key slot is missing".to_string())?;
        if target_key.salt != active.salt {
            return Err("post-handshake KMREQ changed the established salt".into());
        }
        if target_key == *active
            || self
                .slot(even, target)
                .is_some_and(|old| old == &target_key)
        {
            return Err("KMREQ attempted to reuse an existing SEK".into());
        }

        match target {
            KkFlag::EvenKey => *even = Some(target_key),
            KkFlag::OddKey => self.odd = Some(target_key),
            KkFlag::Clear | KkFlag::Invalid => unreachable!("opposite key is encrypted"),
        }
        if self.previous.is_some_and(|old| old.flag == target) {
            self.previous = None;
        }
        self.pending = Some(PendingRotation {
            target,
            request: km.clone(),
        });
        Ok(())
    }

    /// Decrypt with the lifecycle-approved slot and promote a pending slot.
    pub(crate) fn decrypt(
        &mut self,
        even: &Option<SrtCrypto>,
        flag: KkFlag,
        seq_no: u32,
        payload: &mut [u8],
    ) -> Result<bool, String> {
        let pending = self.pending.as_ref().is_some_and(|p| p.target == flag);
        let previous = self
            .previous
            .is_some_and(|p| p.flag == flag && crate::reliability::seq_lt(seq_no, p.switch_seq));
        if flag != self.active && !pending && !previous {
            return Err(format!("SRT {flag:?} packet has no lifecycle-approved key"));
        }
        self.slot(even, flag)
            .ok_or_else(|| format!("SRT {flag:?} key slot is missing"))?
            .decrypt_packet(seq_no, payload);
        if pending {
            let old_active = self.active;
            self.active = flag;
            self.pending = None;
            self.previous = Some(PreviousKey {
                flag: old_active,
                switch_seq: seq_no,
            });
        }
        Ok(pending)
    }
}

fn opposite_key(flag: KkFlag) -> KkFlag {
    match flag {
        KkFlag::EvenKey => KkFlag::OddKey,
        KkFlag::OddKey => KkFlag::EvenKey,
        KkFlag::Clear | KkFlag::Invalid => unreachable!("active key is encrypted"),
    }
}
