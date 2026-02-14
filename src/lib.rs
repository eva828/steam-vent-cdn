//! tModLoader Workshop Download Library
//!
//! Provides constants and utilities for Steam Workshop operations.

// Steam App IDs
pub const TMODLOADER_APP_ID: u32 = 1281930;
pub const TMODLOADER_MOD_BROWSER_APP_ID: u32 = 1281931;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_ids() {
        assert_eq!(TMODLOADER_APP_ID, 1281930);
        assert_eq!(TMODLOADER_MOD_BROWSER_APP_ID, 1281931);
    }
}
