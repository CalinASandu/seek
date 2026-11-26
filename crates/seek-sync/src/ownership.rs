//! Who may write a device's index: the first PC to claim it, and only that PC afterwards.

use uuid::Uuid;

use crate::wire::Refusal;

/// `Ok` when `device` is unknown (first claim) or already owned by `sender`, else `NotOwner`.
pub fn check(owner_of: impl Fn(Uuid) -> Option<Uuid>, sender: Uuid, device: Uuid) -> Result<(), Refusal> {
    match owner_of(device) {
        Some(owner) if owner != sender => Err(Refusal::NotOwner { device }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_device_is_a_first_claim() {
        assert_eq!(check(|_| None, Uuid::from_u128(1), Uuid::from_u128(9)), Ok(()));
    }

    #[test]
    fn owner_may_write() {
        let me = Uuid::from_u128(1);
        assert_eq!(check(|_| Some(me), me, Uuid::from_u128(9)), Ok(()));
    }

    #[test]
    fn other_owner_is_refused() {
        let dev = Uuid::from_u128(9);
        assert_eq!(
            check(|_| Some(Uuid::from_u128(2)), Uuid::from_u128(1), dev),
            Err(Refusal::NotOwner { device: dev })
        );
    }
}
