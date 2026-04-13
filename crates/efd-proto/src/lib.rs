pub mod downstream;
pub mod radio;
pub mod upstream;
pub mod wire;

pub use downstream::{AudioChunk, Capabilities, DrmStatus, ErrorMsg, FftBins, RadioState};
pub use radio::{AgcMode, Mode, SourceKind, Vfo};
pub use upstream::{AudioSource, CatCommand, Ptt, TxAudio};
pub use wire::{decode_msg, encode_msg, ClientMsg, ServerMsg, WireError, PROTO_VERSION};

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::config;

    fn round_trip<T>(val: &T) -> T
    where
        T: bincode::Encode + bincode::Decode<()> + PartialEq + std::fmt::Debug,
    {
        let cfg = config::standard();
        let bytes = bincode::encode_to_vec(val, cfg).expect("encode");
        let (decoded, _): (T, _) = bincode::decode_from_slice(&bytes, cfg).expect("decode");
        decoded
    }

    #[test]
    fn fft_bins_round_trip() {
        let orig = FftBins {
            center_freq_hz: 7_100_000,
            span_hz: 192_000,
            ref_level_db: -30.0,
            bins: vec![-80.0, -75.5, -60.0, -55.2],
            timestamp_us: 123_456_789,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn audio_chunk_round_trip() {
        let orig = AudioChunk {
            opus_data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            seq: 42,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn radio_state_round_trip() {
        let orig = RadioState {
            vfo: Vfo::A,
            freq_hz: 14_200_000,
            mode: Mode::USB,
            filter_bw: "2400".to_string(),
            att: false,
            lp: true,
            agc: AgcMode::Slow,
            agc_threshold: 50,
            nr: false,
            nb: false,
            s_meter_db: -73.0,
            tx: false,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn error_msg_round_trip() {
        let orig = ErrorMsg {
            code: 500,
            message: "rigctld connection lost".to_string(),
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn cat_command_round_trip() {
        let orig = CatCommand {
            raw: "FA00007100000;".to_string(),
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn tx_audio_round_trip() {
        let orig = TxAudio {
            opus_data: vec![1, 2, 3, 4, 5],
            seq: 100,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn ptt_round_trip() {
        let on = Ptt { on: true };
        let off = Ptt { on: false };
        assert_eq!(on, round_trip(&on));
        assert_eq!(off, round_trip(&off));
    }

    #[test]
    fn drm_status_round_trip() {
        let orig = DrmStatus {
            io_ok: true,
            time_ok: true,
            frame_ok: true,
            fac_ok: true,
            sdc_ok: true,
            msc_ok: true,
            if_level_db: Some(-17.8),
            snr_db: Some(25.6),
            wmer_db: Some(23.6),
            mer_db: Some(23.6),
            dc_freq_hz: Some(11_965.99),
            sample_offset_hz: Some(-5.45),
            doppler_hz: Some(1.11),
            delay_ms: Some(0.62),
            robustness_mode: Some("B".into()),
            bandwidth_khz: Some(10),
            sdc_mode: Some("16-QAM".into()),
            msc_mode: Some("SM 64-QAM".into()),
            interleaver_s: Some(2),
            num_audio_services: 1,
            num_data_services: 0,
            timestamp_us: 987_654_321,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn capabilities_round_trip() {
        let orig = Capabilities {
            source: SourceKind::FdmDuo,
            has_iq: true,
            has_tx: true,
            has_hardware_cat: true,
            supported_demod_modes: vec![Mode::USB, Mode::LSB, Mode::CW, Mode::AM, Mode::FM],
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn server_msg_round_trip() {
        let msgs = vec![
            ServerMsg::FftBins(FftBins {
                center_freq_hz: 7_000_000,
                span_hz: 48_000,
                ref_level_db: -20.0,
                bins: vec![-90.0; 4096],
                timestamp_us: 0,
            }),
            ServerMsg::Audio(AudioChunk {
                opus_data: vec![0xFF; 120],
                seq: 1,
            }),
            ServerMsg::RadioState(RadioState {
                vfo: Vfo::B,
                freq_hz: 3_500_000,
                mode: Mode::CW,
                filter_bw: "500".to_string(),
                att: true,
                lp: false,
                agc: AgcMode::Fast,
                agc_threshold: 100,
                nr: true,
                nb: true,
                s_meter_db: -60.0,
                tx: true,
            }),
            ServerMsg::Error(ErrorMsg {
                code: 404,
                message: "not found".to_string(),
            }),
        ];
        for msg in &msgs {
            assert_eq!(*msg, round_trip(msg));
        }
    }

    #[test]
    fn client_msg_round_trip() {
        let msgs = vec![
            ClientMsg::CatCommand(CatCommand {
                raw: "IF;".to_string(),
            }),
            ClientMsg::TxAudio(TxAudio {
                opus_data: vec![0xAB; 60],
                seq: 7,
            }),
            ClientMsg::Ptt(Ptt { on: true }),
        ];
        for msg in &msgs {
            assert_eq!(*msg, round_trip(msg));
        }
    }
}
