//! The stream formats an endpoint offers, the same for every program on
//! these crates: the AVB community's Basic and Extended Audio profiles'
//! AAF, in SR class A and B at 48, 96 and 192 kHz, and IEC 61883-6 AM824
//! in class A, as MOTU's, Apple's and PreSonus's endpoints send; each
//! stream size where its frame fits one Ethernet frame.

use atdecc::stream_format::StreamFormat;

use crate::media::Media;

/// The sampling rates: the Basic Audio profile's 48 kHz, and the Extended
/// Audio profile's 96 and 192.
pub const RATES: [u32; 3] = [48_000, 96_000, 192_000];

/// Frames a second: one each class A interval, as Milan's formats send.
pub const CLASS_A_FRAMES: u32 = 8000;

/// Frames a second: one each class B interval, as the profiles' class B
/// formats and a stream across a Wi-Fi hop send.
pub const CLASS_B_FRAMES: u32 = 4000;

/// The profiles' stream sizes, in channels, largest first.
const SIZES: [u16; 4] = [32, 16, 8, 2];

/// The most octets an AVTPDU takes in one Ethernet frame.
const MOST_PDU: usize = 1500;

/// AAF at `rate`, sent `frames` times a second, in 32-bit integers, `bits`
/// of them used.
pub fn aaf(rate: u32, channels: u16, bits: u8, frames: u32) -> StreamFormat {
    let nsr: u64 = match rate {
        96_000 => 0x07,
        192_000 => 0x09,
        _ => 0x05,
    };
    StreamFormat(
        (0x02 << 56)
            | (nsr << 48)
            | (0x02 << 40)
            | (u64::from(bits) << 32)
            | (u64::from(channels & 0x3ff) << 22)
            | (u64::from(rate / frames) << 12),
    )
}

/// IEC 61883-6 AM824 at `rate`, non-blocking, every quadlet multi-bit
/// linear audio, of `channels` or fewer, as MOTU's endpoints declare it.
pub fn am824(rate: u32, channels: u16) -> StreamFormat {
    let sfc: u64 = match rate {
        96_000 => 0x04,
        192_000 => 0x06,
        _ => 0x02,
    };
    let quadlets = u64::from(channels.min(255));
    StreamFormat((0xa0 << 48) | (sfc << 40) | (quadlets << 32) | (0x60 << 24) | (quadlets << 8))
}

/// Whether a stream in `format` fits one Ethernet frame each frame.
fn fits(format: StreamFormat) -> bool {
    Media::of(format).is_some_and(|media| media.pdu_length() <= MOST_PDU)
}

/// The formats a stream of `channels` offers, the first its default: at
/// its own channels, then the profiles' stream sizes it takes as well, 2
/// and 8 as every endpoint does, and 16 and 32 below a larger stream's
/// own. At each size, class A's formats come first, AAF in 32 bits and in
/// 24 of 32 and AM824, at each rate, then class B's AAF; class B's first
/// for a class B talker, whose default they are. Each where it fits.
pub fn offered(channels: u16, class_b: bool) -> Vec<StreamFormat> {
    let sizes = std::iter::once(channels).chain(
        SIZES
            .into_iter()
            .filter(|size| *size != channels && (*size <= 8 || *size < channels)),
    );
    sizes
        .flat_map(|size| {
            let class_a = RATES.into_iter().flat_map(move |rate| {
                [
                    aaf(rate, size, 32, CLASS_A_FRAMES),
                    aaf(rate, size, 24, CLASS_A_FRAMES),
                    am824(rate, size),
                ]
            });
            let class_b_formats = RATES.into_iter().flat_map(move |rate| {
                [
                    aaf(rate, size, 32, CLASS_B_FRAMES),
                    aaf(rate, size, 24, CLASS_B_FRAMES),
                ]
            });
            let formats: Vec<StreamFormat> = if class_b {
                class_b_formats.chain(class_a).collect()
            } else {
                class_a.chain(class_b_formats).collect()
            };
            formats.into_iter().filter(|format| fits(*format))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atdecc::stream_format::FormatKind;

    /// The largest stream size offered in AAF at `rate` and `frames` a
    /// second.
    fn largest(formats: &[StreamFormat], rate: u32, frames: u32) -> Option<u16> {
        formats
            .iter()
            .copied()
            .filter(|format| matches!(format.kind(), FormatKind::Aaf(_)))
            .filter_map(|format| format.channels().map(|channels| (format, channels)))
            .filter(|(format, channels)| *format == aaf(rate, *channels, 32, frames))
            .map(|(_, channels)| channels)
            .max()
    }

    /// The Extended Audio profile's largest stream size each class
    /// carries, for a stream of 32 channels.
    #[test]
    fn each_class_carries_the_profiles_largest_sizes() {
        let formats = offered(32, false);
        let table = [(48_000, 32, 16), (96_000, 16, 8), (192_000, 8, 2)];
        for (rate, class_a, class_b) in table {
            assert_eq!(
                largest(&formats, rate, CLASS_A_FRAMES),
                Some(class_a),
                "{rate}"
            );
            assert_eq!(
                largest(&formats, rate, CLASS_B_FRAMES),
                Some(class_b),
                "{rate}"
            );
        }
    }

    /// Each class sends a frame each interval, so a frame holds more
    /// samples at the higher rates.
    #[test]
    fn frames_hold_the_profiles_samples() {
        let table = [(48_000, 6, 12), (96_000, 12, 24), (192_000, 24, 48)];
        for (rate, class_a, class_b) in table {
            for (frames, samples) in [(CLASS_A_FRAMES, class_a), (CLASS_B_FRAMES, class_b)] {
                let media = Media::of(aaf(rate, 2, 24, frames)).unwrap();
                assert_eq!(media.samples_per_frame(), samples, "{rate} {frames}");
            }
            assert!(aaf(rate, 2, 24, CLASS_B_FRAMES).is_class_b());
            assert!(!aaf(rate, 2, 24, CLASS_A_FRAMES).is_class_b());
        }
    }

    /// Every stream offers the 2- and 8-channel sizes in both classes at
    /// every rate, in 24 of 32 bits as the profiles have them and in 32,
    /// and AM824 in class A, as MOTU's 8D declares it.
    #[test]
    fn every_stream_offers_2_and_8_channels_everywhere() {
        for channels in [1, 2, 6, 8, 12, 16, 32, 60] {
            for class_b in [false, true] {
                let formats = offered(channels, class_b);
                for rate in RATES {
                    for frames in [CLASS_A_FRAMES, CLASS_B_FRAMES] {
                        for bits in [24, 32] {
                            assert!(formats.contains(&aaf(rate, 2, bits, frames)));
                            let eight = aaf(rate, 8, bits, frames);
                            // At 192 kHz, class B's 8 channels take more than
                            // a frame.
                            assert_eq!(formats.contains(&eight), fits(eight));
                        }
                    }
                    assert!(formats.contains(&am824(rate, 2)));
                    assert!(formats.contains(&am824(rate, 8)));
                }
            }
        }
        assert_eq!(am824(48_000, 8), StreamFormat(0x00a0_0208_6000_0800));
    }

    /// A stream's own channels come first, in class A unless it is a class
    /// B talker.
    #[test]
    fn the_default_is_the_streams_own_size() {
        assert_eq!(offered(8, false)[0], aaf(48_000, 8, 32, CLASS_A_FRAMES));
        assert_eq!(offered(8, true)[0], aaf(48_000, 8, 32, CLASS_B_FRAMES));
        assert_eq!(offered(6, false)[0], aaf(48_000, 6, 32, CLASS_A_FRAMES));
        assert_eq!(offered(8, true).len(), offered(8, false).len());
        // 16 and 32 only below a larger stream's own; 2 and 8 always.
        let sizes = |channels| {
            let mut sizes: Vec<u16> = offered(channels, false)
                .iter()
                .filter_map(|format| format.channels())
                .collect();
            sizes.dedup();
            sizes
        };
        assert_eq!(sizes(2), [2, 8]);
        assert_eq!(sizes(12), [12, 8, 2]);
        assert_eq!(sizes(40), [40, 32, 16, 8, 2]);
    }

    /// However many channels, a stream's formats fit its descriptor in a
    /// READ_DESCRIPTOR response, after its 138 octets before them and the
    /// configuration index and reserved field.
    #[test]
    fn the_formats_fit_a_descriptor() {
        let room = (atdecc::aem::MAX_AEM_PAYLOAD - 4 - 138) / 8;
        for channels in 1..=60 {
            for class_b in [false, true] {
                let count = offered(channels, class_b).len();
                assert!(count <= room, "{channels} channels: {count} formats");
            }
        }
    }

    /// Every format offered is one the talker and listener stream.
    #[test]
    fn every_format_streams() {
        for channels in 1..=60 {
            for format in offered(channels, false) {
                let media = Media::of(format).unwrap();
                assert!(media.pdu_length() <= MOST_PDU);
            }
        }
    }
}
