pub mod downstream;
pub mod grid;
pub mod radio;
pub mod upstream;
pub mod wire;

pub use downstream::{
    AudioChunk, Capabilities, ControlTarget, DecodedText, DeviceList, DrmStatus, ErrorMsg, FftBins,
    RadioState, RecordingStatus, StateSnapshot,
};
pub use grid::GridCell;
pub use radio::{AgcMode, DecoderKind, DeviceId, Mode, RecKind, SourceClass, SourceKind, Vfo};
pub use upstream::{CatCommand, Ptt, StartRecording, TxAudio};
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

    fn sample_radio_state() -> RadioState {
        RadioState {
            vfo: Vfo::A,
            freq_hz: 14_200_000,
            mode: Mode::USB,
            filter_bw: "2400".to_string(),
            filter_bw_hz: Some(2400.0),
            att: false,
            lp: true,
            agc: AgcMode::Slow,
            agc_threshold: 50,
            nr: false,
            nb: false,
            s_meter_db: -73.0,
            tx: false,
            rit_hz: 0,
            rit_on: false,
            xit_hz: 0,
            xit_on: false,
            if_offset_hz: 0,
            snr_db: Some(18.5),
        }
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
        let orig = sample_radio_state();
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
            has_usb_audio: true,
            supported_demod_modes: vec![Mode::USB, Mode::LSB, Mode::CW, Mode::AM, Mode::SAM, Mode::FM],
            supported_decoders: vec![DecoderKind::Cw, DecoderKind::Rtty, DecoderKind::Psk],
            drm_flip_spectrum: false,
            control_target: ControlTarget::DemodMirrorFreq,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn device_list_round_trip() {
        let orig = DeviceList {
            audio_devices: vec![DeviceId {
                kind: SourceKind::PortableRadio,
                id: "hw:1,0".into(),
            }],
            iq_devices: vec![
                DeviceId {
                    kind: SourceKind::FdmDuo,
                    id: "SL1JO3".into(),
                },
                DeviceId {
                    kind: SourceKind::RtlSdr,
                    id: "0".into(),
                },
            ],
            active: Some(DeviceId {
                kind: SourceKind::FdmDuo,
                id: "SL1JO3".into(),
            }),
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn decoded_text_round_trip() {
        let orig = DecodedText {
            decoder: DecoderKind::Cw,
            text: "QST DE WA1W".into(),
            timestamp_us: 123,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn recording_status_round_trip() {
        let orig = RecordingStatus {
            active: true,
            kind: Some(RecKind::Iq),
            path: Some("/var/efd/rec/20260418-1400-iq.raw".into()),
            bytes_written: 123_456,
            duration_s: Some(12.5),
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn state_snapshot_round_trip() {
        let orig = StateSnapshot {
            active_device: Some(DeviceId {
                kind: SourceKind::FdmDuo,
                id: "SL1JO3".into(),
            }),
            freq_hz: 14_074_000,
            mode: Mode::USB,
            filter_bw_hz: Some(2400.0),
            rit_hz: 10,
            xit_hz: 0,
            if_offset_hz: -15,
            enabled_decoders: vec![DecoderKind::Cw, DecoderKind::Ft8],
            nb_on: true,
            dnb_on: false,
            dnr_on: true,
            dnf_on: false,
            apf_on: false,
        };
        assert_eq!(orig, round_trip(&orig));
    }

    #[test]
    fn grid_cell_round_trip() {
        let cells = [
            GridCell::Disp0Center,
            GridCell::Ctrl1Right,
            GridCell::Spectrum,
            GridCell::TimeAxis,
        ];
        for cell in cells {
            assert_eq!(cell, round_trip(&cell));
        }
    }

    #[test]
    fn source_kind_class_mapping() {
        assert_eq!(SourceKind::FdmDuo.class(), SourceClass::Iq);
        assert_eq!(SourceKind::HackRf.class(), SourceClass::Iq);
        assert_eq!(SourceKind::IqFile.class(), SourceClass::Iq);
        assert_eq!(SourceKind::PortableRadio.class(), SourceClass::Audio);
        assert_eq!(SourceKind::AudioFile.class(), SourceClass::Audio);
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
            ServerMsg::RadioState(sample_radio_state()),
            ServerMsg::Error(ErrorMsg {
                code: 404,
                message: "not found".to_string(),
            }),
            ServerMsg::DeviceList(DeviceList {
                audio_devices: vec![],
                iq_devices: vec![],
                active: None,
            }),
            ServerMsg::DecodedText(DecodedText {
                decoder: DecoderKind::Rtty,
                text: "CQ CQ".into(),
                timestamp_us: 9,
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
            ClientMsg::SetDemodMode(Some(Mode::SAMU)),
            ClientMsg::SetDrmFlipSpectrum(true),
            ClientMsg::EnumerateDevices,
            ClientMsg::SelectSource(SourceClass::Iq),
            ClientMsg::SelectDevice(DeviceId {
                kind: SourceKind::HackRf,
                id: "abcd".into(),
            }),
            ClientMsg::SetDecoder {
                decoder: DecoderKind::Ft8,
                enabled: true,
            },
            ClientMsg::SetNb(true),
            ClientMsg::SetDnr(true),
            ClientMsg::SetDnf(false),
            ClientMsg::SetApf(true),
            ClientMsg::SetDnb(false),
            ClientMsg::StartRecording(StartRecording {
                kind: RecKind::Audio,
                path: None,
            }),
            ClientMsg::StopRecording,
            ClientMsg::SaveState,
            ClientMsg::LoadState,
        ];
        for msg in &msgs {
            assert_eq!(*msg, round_trip(msg));
        }
    }
}
