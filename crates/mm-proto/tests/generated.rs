#![allow(deprecated)]
use mm_domain::OpSpec;
use mm_proto::v1;
use prost::Message;

type GeneratedClient =
    v1::media_job_service_client::MediaJobServiceClient<tonic::transport::Channel>;

fn assert_server_trait<T: v1::media_job_service_server::MediaJobService>() -> &'static str {
    std::any::type_name::<T>()
}

#[test]
fn client_type_is_generated() {
    let name = std::any::type_name::<GeneratedClient>();
    assert!(name.contains("MediaJobServiceClient"));
}

#[test]
fn server_trait_is_generated() {
    let _ = assert_server_trait::<DummyService>;
}

struct DummyService;

#[tonic::async_trait]
impl v1::media_job_service_server::MediaJobService for DummyService {
    type WatchJobStream = tonic::codegen::BoxStream<v1::WatchJobResponse>;

    async fn create_job(
        &self,
        _request: tonic::Request<v1::CreateJobRequest>,
    ) -> Result<tonic::Response<v1::CreateJobResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn get_job(
        &self,
        _request: tonic::Request<v1::GetJobRequest>,
    ) -> Result<tonic::Response<v1::GetJobResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn cancel_job(
        &self,
        _request: tonic::Request<v1::CancelJobRequest>,
    ) -> Result<tonic::Response<v1::CancelJobResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn list_jobs(
        &self,
        _request: tonic::Request<v1::ListJobsRequest>,
    ) -> Result<tonic::Response<v1::ListJobsResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn watch_job(
        &self,
        _request: tonic::Request<v1::WatchJobRequest>,
    ) -> Result<tonic::Response<Self::WatchJobStream>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn create_template(
        &self,
        _request: tonic::Request<v1::CreateTemplateRequest>,
    ) -> Result<tonic::Response<v1::CreateTemplateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn get_template(
        &self,
        _request: tonic::Request<v1::GetTemplateRequest>,
    ) -> Result<tonic::Response<v1::GetTemplateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn list_templates(
        &self,
        _request: tonic::Request<v1::ListTemplatesRequest>,
    ) -> Result<tonic::Response<v1::ListTemplatesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn delete_template(
        &self,
        _request: tonic::Request<v1::DeleteTemplateRequest>,
    ) -> Result<tonic::Response<v1::DeleteTemplateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn render_template(
        &self,
        _request: tonic::Request<v1::RenderTemplateRequest>,
    ) -> Result<tonic::Response<v1::RenderTemplateResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }

    async fn create_upload_target(
        &self,
        _request: tonic::Request<v1::CreateUploadTargetRequest>,
    ) -> Result<tonic::Response<v1::CreateUploadTargetResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(""))
    }
}

#[test]
fn create_job_request_encodes_and_decodes() {
    let request = v1::CreateJobRequest {
        user_id: "user-1".to_string(),
        idempotency_key: "key-1".to_string(),
        spec: Some(v1::OpSpec {
            op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
                input: Some(v1::InputSource {
                    source: Some(v1::input_source::Source::Media(v1::MediaSource {
                        media: Some(v1::MediaRef {
                            r#ref: Some(v1::media_ref::Ref::ObjectUri(
                                "s3://bucket/key".to_string(),
                            )),
                        }),
                        trim: None,
                        speed: None,
                        volume: None,
                    })),
                }),
            })),
        }),
        callback: Some(v1::Callback {
            queue_url: "https://queue.example/q".to_string(),
        }),
    };
    let bytes = request.encode_to_vec();
    let decoded = v1::CreateJobRequest::decode(bytes.as_slice()).unwrap();
    assert_eq!(request, decoded);
}

#[test]
fn create_job_response_carries_replay_fields() {
    let response = v1::CreateJobResponse {
        job_id: "0199a7a2-0000-7000-8000-000000000000".to_string(),
        request_id: "req-1".to_string(),
        created: false,
    };
    let decoded = v1::CreateJobResponse::decode(response.encode_to_vec().as_slice()).unwrap();
    assert!(!decoded.created);
    assert_eq!(decoded.request_id, "req-1");
}

#[test]
fn list_jobs_request_takes_statuses_and_cursor() {
    let request = v1::ListJobsRequest {
        user_id: "user-1".to_string(),
        statuses: vec![v1::JobState::Pending as i32, v1::JobState::Running as i32],
        page_size: 10,
        page_token: "b64cursor".to_string(),
    };
    let decoded = v1::ListJobsRequest::decode(request.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.statuses.len(), 2);
    assert_eq!(decoded.page_token, "b64cursor");
}

#[test]
fn job_event_distinguishes_state_and_progress() {
    let state_event = v1::JobEvent {
        kind: Some(v1::job_event::Kind::State(v1::JobState::Running as i32)),
    };
    let progress_event = v1::JobEvent {
        kind: Some(v1::job_event::Kind::ProgressPercent(42)),
    };
    assert_ne!(state_event, progress_event);
    let decoded = v1::JobEvent::decode(progress_event.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.kind, Some(v1::job_event::Kind::ProgressPercent(42)));
}

#[test]
fn job_state_enum_covers_lifecycle() {
    let states = [
        v1::JobState::Unspecified,
        v1::JobState::Pending,
        v1::JobState::Running,
        v1::JobState::Succeeded,
        v1::JobState::Failed,
        v1::JobState::Cancelled,
    ];
    for state in states {
        assert_eq!(v1::JobState::try_from(state as i32).unwrap(), state);
    }
}

#[test]
fn op_spec_oneof_carries_every_arm() {
    let media = || v1::InputSource {
        source: Some(v1::input_source::Source::Media(v1::MediaSource {
            media: Some(v1::MediaRef {
                r#ref: Some(v1::media_ref::Ref::HttpUrl(
                    "https://host/input.mp4".to_string(),
                )),
            }),
            trim: None,
            speed: None,
            volume: None,
        })),
    };
    let arms = [
        v1::op_spec::Op::Concat(v1::ConcatSpec {
            inputs: vec![media(), media()],
            profile: None,
        }),
        v1::op_spec::Op::Probe(v1::ProbeSpec {
            input: Some(media()),
        }),
        v1::op_spec::Op::Transcode(v1::TranscodeSpec {
            input: Some(media()),
            profile: Some(v1::OutputProfile {
                frame_rate: 30.0,
                video_max_bitrate: 4_000_000,
                video_quality: 23,
                audio_bitrate: 128_000,
                audio_sample_rate: 48_000,
                width: 1280,
                height: 720,
                poster_seconds: None,
                range: None,
                video_encoder: v1::VideoEncoder::Libx264 as i32,
                require_hardware: false,
            }),
        }),
        v1::op_spec::Op::Trim(v1::TrimSpec {
            input: Some(media()),
            start_seconds: 1.0,
            duration_seconds: 2.0,
        }),
        v1::op_spec::Op::Thumbnail(v1::ThumbnailSpec {
            input: Some(media()),
            offsets_seconds: vec![0.0, 1.5],
            width: 320,
            height: 180,
            sprite_columns: 0,
        }),
        v1::op_spec::Op::Image(v1::ImageSpec {
            inputs: vec![media()],
            op: v1::ImageOp::Resize as i32,
            width: 320,
            height: 180,
            resize_mode: v1::ResizeMode::Fit as i32,
            format: v1::ImageFormat::Unspecified as i32,
        }),
        v1::op_spec::Op::Package(v1::PackageSpec {
            input: Some(media()),
            segment_seconds: 6.0,
        }),
        v1::op_spec::Op::Batch(v1::BatchSpec {
            steps: vec![v1::OpSpec {
                op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
                    input: Some(media()),
                })),
            }],
            stop_on_failure: Some(true),
        }),
        v1::op_spec::Op::Graph(v1::GraphSpec {
            nodes: vec![v1::GraphNode {
                id: "only".to_string(),
                spec: Some(v1::OpSpec {
                    op: Some(v1::op_spec::Op::Probe(v1::ProbeSpec {
                        input: Some(media()),
                    })),
                }),
            }],
        }),
        v1::op_spec::Op::Title(v1::TitleSpec {
            text: "Hello".to_string(),
            width: 320,
            height: 120,
            point_size: 24,
            colour: "#ffffff".to_string(),
            background: Some("#112233".to_string()),
            format: v1::ImageFormat::Png as i32,
        }),
        v1::op_spec::Op::Audio(v1::AudioSpec {
            inputs: vec![media()],
            gain_db: 6.0,
            fade_in_ticks: 1,
            fade_out_ticks: 2,
            high_pass_hz: Some(80),
            low_pass_hz: Some(12000),
            limiter_true_peak_db: Some(-1.0),
        }),
        v1::op_spec::Op::Grade(v1::GradeSpec {
            input: Some(media()),
            adjustments: Some(v1::GradeAdjustments {
                brightness: 0.1,
                contrast: 1.2,
                saturation: 1.3,
                gamma: 1.1,
            }),
            lookup_table: None,
            scope: Some(v1::GradeScope {
                width: 320,
                height: 180,
                mode: v1::ScopeMode::Waveform as i32,
                output: v1::ScopeOutput::Data as i32,
                columns: 128,
                bins: 32,
            }),
        }),
        v1::op_spec::Op::Subtitle(v1::SubtitleSpec {
            input: Some(media()),
            captions: Some(media()),
            format: v1::CaptionFormat::Srt as i32,
            mode: v1::SubtitleMode::Burn as i32,
        }),
        v1::op_spec::Op::Loudness(v1::LoudnessSpec {
            input: Some(media()),
            target: Some(v1::LoudnessTarget {
                integrated_lufs: -16.0,
                true_peak_db: -1.5,
            }),
        }),
        v1::op_spec::Op::Ladder(v1::LadderSpec {
            input: Some(media()),
            rungs: vec![
                v1::OutputProfile {
                    video_max_bitrate: 800_000,
                    audio_bitrate: 96_000,
                    width: 640,
                    height: 360,
                    ..Default::default()
                },
                v1::OutputProfile {
                    video_max_bitrate: 2_500_000,
                    audio_bitrate: 96_000,
                    width: 1280,
                    height: 720,
                    ..Default::default()
                },
            ],
            segment_seconds: Some(6.0),
            poster_seconds: None,
        }),
    ];
    assert_eq!(arms.len(), 15);
    for arm in arms {
        assert!(OpSpec::try_from(v1::OpSpec { op: Some(arm) }).is_ok());
    }
}

#[test]
fn file_error_pairs_ref_and_code() {
    let error = v1::FileError {
        media_ref: Some(v1::MediaRef {
            r#ref: Some(v1::media_ref::Ref::ObjectUri("s3://b/k".to_string())),
        }),
        code: "NO_VIDEO".to_string(),
        step_index: Some(1),
        node_id: Some("join".to_string()),
        input_index: Some(0),
        rung: None,
    };
    let decoded = v1::FileError::decode(error.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.code, "NO_VIDEO");
    assert_eq!(decoded.step_index, Some(1));
    assert_eq!(decoded.node_id.as_deref(), Some("join"));
    assert_eq!(decoded.input_index, Some(0));
}
