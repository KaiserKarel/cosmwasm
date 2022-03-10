use wasmer::{Array, ValueType, WasmPtr};

use crate::conversion::to_u32;
use crate::errors::{
    CommunicationError, CommunicationResult, RegionValidationError, RegionValidationResult,
    VmResult,
};
use crate::wasm::Memory;

/****** read/write to wasm memory buffer ****/

/// Describes some data allocated in Wasm's linear memory.
/// A pointer to an instance of this can be returned over FFI boundaries.
///
/// This is the same as `cosmwasm_std::memory::Region`
/// but defined here to allow Wasmer specific implementation.
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct Region {
    /// The beginning of the region expressed as bytes from the beginning of the linear memory
    pub offset: u32,
    /// The number of bytes available in this region
    pub capacity: u32,
    /// The number of bytes used in this region
    pub length: u32,
}

unsafe impl ValueType for Region {}

/// Performs plausibility checks in the given Region. Regions are always created by the
/// contract and this can be used to detect problems in the standard library of the contract.
pub fn validate_region(region: &Region) -> RegionValidationResult<()> {
    if region.offset == 0 {
        return Err(RegionValidationError::zero_offset());
    }
    if region.length > region.capacity {
        return Err(RegionValidationError::length_exceeds_capacity(
            region.length,
            region.capacity,
        ));
    }
    if region.capacity > (u32::MAX - region.offset) {
        return Err(RegionValidationError::out_of_range(
            region.offset,
            region.capacity,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_region_passes_for_valid_region() {
        // empty
        let region = Region {
            offset: 23,
            capacity: 500,
            length: 0,
        };
        validate_region(&region).unwrap();

        // half full
        let region = Region {
            offset: 23,
            capacity: 500,
            length: 250,
        };
        validate_region(&region).unwrap();

        // full
        let region = Region {
            offset: 23,
            capacity: 500,
            length: 500,
        };
        validate_region(&region).unwrap();

        // at end of linear memory (1)
        let region = Region {
            offset: u32::MAX,
            capacity: 0,
            length: 0,
        };
        validate_region(&region).unwrap();

        // at end of linear memory (2)
        let region = Region {
            offset: 1,
            capacity: u32::MAX - 1,
            length: 0,
        };
        validate_region(&region).unwrap();
    }

    #[test]
    fn validate_region_fails_for_zero_offset() {
        let region = Region {
            offset: 0,
            capacity: 500,
            length: 250,
        };
        let result = validate_region(&region);
        match result.unwrap_err() {
            RegionValidationError::ZeroOffset { .. } => {}
            e => panic!("Got unexpected error: {:?}", e),
        }
    }

    #[test]
    fn validate_region_fails_for_length_exceeding_capacity() {
        let region = Region {
            offset: 23,
            capacity: 500,
            length: 501,
        };
        let result = validate_region(&region);
        match result.unwrap_err() {
            RegionValidationError::LengthExceedsCapacity {
                length, capacity, ..
            } => {
                assert_eq!(length, 501);
                assert_eq!(capacity, 500);
            }
            e => panic!("Got unexpected error: {:?}", e),
        }
    }

    #[test]
    fn validate_region_fails_when_exceeding_address_space() {
        let region = Region {
            offset: 23,
            capacity: u32::MAX,
            length: 501,
        };
        let result = validate_region(&region);
        match result.unwrap_err() {
            RegionValidationError::OutOfRange {
                offset, capacity, ..
            } => {
                assert_eq!(offset, 23);
                assert_eq!(capacity, u32::MAX);
            }
            e => panic!("Got unexpected error: {:?}", e),
        }

        let region = Region {
            offset: u32::MAX,
            capacity: 1,
            length: 0,
        };
        let result = validate_region(&region);
        match result.unwrap_err() {
            RegionValidationError::OutOfRange {
                offset, capacity, ..
            } => {
                assert_eq!(offset, u32::MAX);
                assert_eq!(capacity, 1);
            }
            e => panic!("Got unexpected error: {:?}", e),
        }
    }
}
