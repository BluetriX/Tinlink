//! Minimal H.264 Annex B helpers: enough to tell keyframes apart and to make
//! sure every keyframe carries the parameter sets a new viewer needs.

/// NAL unit types (ITU-T H.264, table 7-1) used here.
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Iterates over the NAL unit types of an Annex B byte stream.
pub fn nal_types(stream: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 <= stream.len() {
            // Matches both 3-byte (00 00 01) and 4-byte (00 00 00 01) start codes.
            if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
                i += 3;
                if let Some(header) = stream.get(i) {
                    return Some(header & 0x1f);
                }
                return None;
            }
            i += 1;
        }
        None
    })
}

pub fn contains_nal(stream: &[u8], nal_type: u8) -> bool {
    nal_types(stream).any(|t| t == nal_type)
}

pub fn is_keyframe(stream: &[u8]) -> bool {
    contains_nal(stream, NAL_IDR)
}

/// Prepends `parameter_sets` (SPS and PPS in Annex B form) to a keyframe
/// that lacks them; other frames are returned unchanged.
pub fn with_parameter_sets(frame: Vec<u8>, parameter_sets: &[u8]) -> Vec<u8> {
    if parameter_sets.is_empty() || !is_keyframe(&frame) || contains_nal(&frame, NAL_SPS) {
        return frame;
    }
    let mut out = Vec::with_capacity(parameter_sets.len() + frame.len());
    out.extend_from_slice(parameter_sets);
    out.extend_from_slice(&frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0, 0, 1, 0x65, 0x88, 0x84];
    const P_SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn lists_nal_types_with_both_start_code_lengths() {
        let stream = concat(&[SPS, PPS, IDR]);
        assert_eq!(nal_types(&stream).collect::<Vec<_>>(), [7, 8, 5]);
        assert!(nal_types(&[]).next().is_none());
        assert!(nal_types(&[0, 0, 1]).next().is_none());
    }

    #[test]
    fn detects_keyframes() {
        assert!(is_keyframe(&concat(&[SPS, PPS, IDR])));
        assert!(!is_keyframe(P_SLICE));
    }

    #[test]
    fn adds_parameter_sets_only_where_missing() {
        let sets = concat(&[SPS, PPS]);
        assert_eq!(
            with_parameter_sets(IDR.to_vec(), &sets),
            concat(&[SPS, PPS, IDR])
        );
        let complete = concat(&[SPS, PPS, IDR]);
        assert_eq!(with_parameter_sets(complete.clone(), &sets), complete);
        assert_eq!(with_parameter_sets(P_SLICE.to_vec(), &sets), P_SLICE);
        assert_eq!(with_parameter_sets(IDR.to_vec(), &[]), IDR);
    }
}
