use std::future::Future;
use std::{pin::Pin, sync::Arc};

use dashmap::DashMap;
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, mpsc};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use webrtc::error::Error as RTCError;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::{RTCPFeedback, RTCRtpTransceiverInit};
use webrtc::track::track_remote::TrackRemote;

use super::downtrack::{DownTrack, DownTrackInternal};
use super::error::Result;
use super::receiver::{Receiver, RtcpDataReceiver, RtcpDataSender, WebRTCReceiver};
use crate::buffer::buffer::Options as BufferOptions;
use crate::rtc::peer::subscriber::Subscriber;
use crate::rtc::room::{Room, RoomEvent};
use crate::track::audio_observer::AudioObserver;
use crate::{buffer::buffer::BufferIO, buffer::factory::AtomicFactory, rtc::config::RouterConfig};

pub type RtcpWriterFn = Box<
    dyn (FnMut(
            Vec<Box<dyn RtcpPacket + Send + Sync>>,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
pub type OnAddReciverTrackFn = Box<
    dyn (FnMut(
            Arc<dyn Receiver + Send + Sync>,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
pub type OnDelReciverTrackFn = Box<
    dyn (FnMut(
            Arc<dyn Receiver + Send + Sync>,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>)
        + Send
        + Sync,
>;
pub struct LocalRouter {
    id: String,
    pub audio_observer: Arc<Mutex<AudioObserver>>,
    //twcc: Arc<Mutex<Option<Responder>>>,
    rtcp_sender_channel: Arc<RtcpDataSender>,
    rtcp_receiver_channel: Arc<Mutex<RtcpDataReceiver>>,
    stop_sender_channel: Arc<Sender<()>>,
    config: RouterConfig,
    receivers: Arc<Mutex<DashMap<String, Arc<dyn Receiver + Send + Sync>>>>,
    buffer_factory: AtomicFactory,
    rtcp_writer_handler: Arc<Mutex<Option<RtcpWriterFn>>>,
    room: Arc<Room>,
    on_add_receiver_track_handler: Arc<Mutex<Option<OnAddReciverTrackFn>>>,
    on_del_receiver_track_handler: Arc<Mutex<Option<OnDelReciverTrackFn>>>,
}

impl LocalRouter {
    pub fn new(id: String, room: Arc<Room>, config: RouterConfig) -> Self {
        let (s, r) = mpsc::channel(1024);
        let (sender, _) = broadcast::channel(1);
        let audio_threshold = config.audio_level_threshold;
        let audio_interval = config.audio_level_interval;
        let audio_filter = config.audio_level_filter;
        let audio_observer = AudioObserver::new(audio_threshold, audio_interval, audio_filter);
        Self {
            id,
            //twcc: Arc::new(Mutex::new(None)),
            //stats: Arc::new(Mutex::new(HashMap::new())),
            audio_observer: Arc::new(Mutex::new(audio_observer)),
            rtcp_sender_channel: Arc::new(s),
            rtcp_receiver_channel: Arc::new(Mutex::new(r)),
            stop_sender_channel: Arc::new(sender),
            config,
            receivers: Arc::new(Mutex::new(DashMap::new())),
            room,
            buffer_factory: AtomicFactory::new(100, 100),
            rtcp_writer_handler: Arc::new(Mutex::new(None)),
            on_add_receiver_track_handler: Arc::new(Mutex::new(None)),
            on_del_receiver_track_handler: Arc::new(Mutex::new(None)),
        }
    }

    /*
    fn get_receivers(&self) -> Arc<Mutex<DashMap<String, Arc<dyn Receiver + Send + Sync>>>> {
        self.receivers.clone()
    }*/

    pub async fn register_on_add_receiver_track(&self, f: OnAddReciverTrackFn) {
        let mut handler = self.on_add_receiver_track_handler.lock().await;
        *handler = Some(f);
    }
    pub async fn register_on_del_receiver_track(&self, f: OnDelReciverTrackFn) {
        let mut handler = self.on_del_receiver_track_handler.lock().await;
        *handler = Some(f);
    }

    pub fn id(&self) -> String {
        self.id.clone()
    }

    pub async fn add_down_track(
        &self,
        subscriber: Arc<Subscriber>,
        receiver: Arc<dyn Receiver>,
    ) -> Result<Option<Arc<DownTrack>>> {
        let downtracks = subscriber.get_tracks(&receiver.stream_id()).await;
        // Checks for available tracks
        if let Some(downtracks_data) = downtracks {
            for dt in downtracks_data {
                // Returns track if exists
                if dt.id() == receiver.track_id() {
                    info!("LocalRouter::add_down_track Downtrack exists");
                    return Ok(Some(dt));
                }
            }
        }

        let codec = receiver.codec();
        subscriber
            .m
            .lock()
            .await
            .register_codec(codec.clone(), receiver.kind())?;
        let codec_capability = RTCRtpCodecCapability {
            mime_type: codec.capability.mime_type,
            clock_rate: codec.capability.clock_rate,
            channels: codec.capability.channels,
            sdp_fmtp_line: codec.capability.sdp_fmtp_line,
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: String::from("goog-remb"),
                    parameter: String::from(""),
                },
                RTCPFeedback {
                    typ: String::from("nack"),
                    parameter: String::from(""),
                },
                RTCPFeedback {
                    typ: String::from("nack"),
                    parameter: String::from("pli"),
                },
            ],
        };

        // New local down track
        let down_track_local = Arc::new(
            DownTrackInternal::new(
                codec_capability,
                receiver.clone(),
                self.config.max_packet_track,
            )
            .await,
        );
        let transceiver = subscriber
            .pc
            .add_transceiver_from_track(
                down_track_local.clone(),
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await?;
        // New local track
        info!("LocalRouter::add_down_track Creating new local track");
        let mut down_track = DownTrack::new_track_local(subscriber.id.clone(), down_track_local);
        down_track.set_transceiver(transceiver.clone());
        let down_track_arc = Arc::new(down_track);

        let s_out = subscriber.clone();
        let r_out = receiver.clone();
        let down_track_out = down_track_arc.clone();
        down_track_arc
            .register_on_close(Box::new(move || {
                let s_in = s_out.clone();
                let r_in = r_out.clone();
                let transceiver_in = transceiver.clone();
                let down_track_in = down_track_out.clone();
                Box::pin(async move {
                    if s_in.pc.connection_state() != RTCPeerConnectionState::Closed {
                        // Remove track from subscriber peer connection
                        let rv = s_in.pc.remove_track(&transceiver_in.sender().await).await;
                        match rv {
                            Ok(_) => {
                                info!("Remove DownTrack for {}", &r_in.stream_id());
                                s_in.remove_down_track(&r_in.stream_id(), &down_track_in)
                                    .await;
                                info!("RemoveDownTrack Negotiate");
                                if let Err(err) = s_in.negotiate(None).await {
                                    error!("negotiate err:{} ", err);
                                }
                            }
                            Err(err) => {
                                if err == RTCError::ErrConnectionClosed {
                                    // return;
                                }
                            }
                        }
                    }
                })
            }))
            .await;

        let s_out_1 = subscriber.clone();
        let r_out_1 = receiver.clone();
        down_track_arc
            .register_on_bind(Box::new(move || {
                let s_in = s_out_1.clone();
                let r_in = r_out_1.clone();

                Box::pin(async move {
                    tokio::spawn(async move {
                        s_in.send_stream_down_track_reports(&r_in.stream_id()).await;
                    });
                })
            }))
            .await;

        subscriber
            .add_down_track(receiver.stream_id(), down_track_arc.clone())
            .await;
        receiver
            .add_down_track(down_track_arc, self.config.simulcast.best_quality_first)
            .await;

        Ok(None)
    }

    pub async fn add_down_tracks(
        &self,
        subscriber: Arc<Subscriber>,
        r: Option<Arc<dyn Receiver>>,
    ) -> Result<()> {
        if subscriber.no_auto_subscribe {
            info!("Skipping no auto subscribe");
            return Ok(());
        }

        if let Some(receiver) = r {
            info!(
                "Add actual downtrack to subscriber, subscriber: {}",
                subscriber.id
            );
            self.add_down_track(subscriber.clone(), receiver).await?;
            subscriber.negotiate(None).await?;
            return Ok(());
        }

        let mut recs = Vec::new();
        {
            let receivers = self.receivers.lock().await;
            for receiver in receivers.iter() {
                recs.push(receiver.clone())
            }
        }

        if !recs.is_empty() {
            info!("Add downtracks from stored receivers to subscriber");
            for val in recs {
                self.add_down_track(subscriber.clone(), val.clone()).await?;
            }
            subscriber.negotiate(None).await?;
        }

        Ok(())
    }

    pub async fn add_receiver(
        self: &Arc<Self>,
        receiver: Arc<RTCRtpReceiver>,
        track: Arc<TrackRemote>,
        track_id: String,
        stream_id: String,
    ) -> (Arc<dyn Receiver>, bool) {
        info!(
            "add_receiver -> track {}, stream: {}",
            track.id(),
            stream_id
        );
        let mut published = false;
        let buffer = self.buffer_factory.get_or_new_buffer(track.ssrc()).await;
        let sender = self.rtcp_sender_channel.clone();
        buffer
            .register_on_feedback(Box::new(
                move |packets: Vec<Box<dyn RtcpPacket + Send + Sync>>| {
                    let sender_in = Arc::clone(&sender);
                    Box::pin(async move {
                        if let Err(err) = sender_in.send(packets).await {
                            error!("send err: {}", err);
                        }
                    })
                },
            ))
            .await;

        match track.kind() {
            RTPCodecType::Audio => {
                let router_out = self.clone();
                let stream_id_out = stream_id.clone();
                buffer
                    .register_on_audio_level(Box::new(move |voice, level| {
                        let router_in = router_out.clone();
                        let stream_id_in = stream_id_out.clone();
                        Box::pin(async move {
                            if !voice {
                                debug!("Skip observation");
                                return;
                            }
                            router_in
                                .audio_observer
                                .lock()
                                .await
                                .observe(&stream_id_in, level)
                                .await;
                        })
                    }))
                    .await;
                debug!(
                    "[Room {}] add stream {} to audio observer",
                    self.room.id, stream_id
                );
                let router_2 = self.clone();
                router_2
                    .audio_observer
                    .lock()
                    .await
                    .add_stream(stream_id)
                    .await;
            }
            RTPCodecType::Video => {
                debug!("Video tracking not implemented");
            }
            _ => {}
        }

        // TODO: implement twcc
        // TODO: implement stats

        let rtcp_reader = self
            .buffer_factory
            .get_or_new_rtcp_buffer(track.ssrc())
            .await;
        //let stats_out = Arc::clone(&self.stats);
        let buffer_out = Arc::clone(&buffer);
        let with_status = self.config.with_stats;
        rtcp_reader
            .lock()
            .await
            .register_on_packet(Box::new(move |packet: Vec<u8>| {
                let buffer_in = Arc::clone(&buffer_out);
                Box::pin(async move {
                    let mut buf = &packet[..];
                    let pkts_result = webrtc::rtcp::packet::unmarshal(&mut buf)?;
                    for pkt in pkts_result {
                        if let Some(description) =
                        pkt.as_any()
                            .downcast_ref::<webrtc::rtcp::source_description::SourceDescription>()
                    {
                        // TODO: send stats
                        info!("Packet SourceDescription: {}", description.to_string());
                    }
                    else if let Some(sender_report) =
                        pkt.as_any()
                            .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                    {
                        info!("Packet SenderReport {}", sender_report.to_string());
                        buffer_in
                            .set_sender_report_data(
                                sender_report.rtp_time,
                                sender_report.ntp_time,
                            )
                            .await;
                        if with_status {
                            // TODO: update stats
                        }
                    }
                    }
                    Ok(())
                })
            }))
            .await;
        let receivers = self.receivers.lock().await;
        let result_receiver;
        match receivers.get(&track.id()) {
            Some(r) => result_receiver = r.clone(),
            None => {
                let mut rv =
                    WebRTCReceiver::new(receiver.clone(), track.clone(), self.id.clone()).await;
                rv.set_rtcp_channel(self.rtcp_sender_channel.clone());
                let recv_kind = rv.kind();
                let stream_id = track.stream_id();
                let router_out = self.clone();
                rv.register_on_close(Box::new(move || {
                    let router_in = router_out.clone();
                    let stream_id_in = stream_id.clone();
                    Box::pin(async move {
                        if recv_kind == RTPCodecType::Audio {
                            router_in
                                .audio_observer
                                .lock()
                                .await
                                .remove_stream(&stream_id_in)
                                .await;
                        }
                    })
                }))
                .await;
                result_receiver = Arc::new(rv);
                receivers.insert(track_id, result_receiver.clone());
                published = true;
                info!("Track {} published", track.id());
                if let Some(f) = &mut *self.on_add_receiver_track_handler.lock().await {
                    let _ = f(result_receiver.clone()).await;
                }
            }
        }
        let layer = result_receiver
            .add_up_track(
                track.clone(),
                buffer.clone(),
                self.config.simulcast.best_quality_first,
            )
            .await;

        if let Some(layer_val) = layer {
            let receiver_clone = result_receiver.clone();
            tokio::spawn(async move { receiver_clone.write_rtp(layer_val).await });
        }

        buffer
            .bind(
                receiver.get_parameters().await,
                BufferOptions {
                    max_bitrate: self.config.max_bandwidth,
                },
            )
            .await;
        let buffer_clone = buffer.clone();
        tokio::spawn(async move {
            let mut b = vec![0u8; 1500];

            while let Ok((pkt, _)) = track.read(&mut b).await {
                if let Err(err) = buffer_clone.write(pkt).await {
                    error!("write error: {}", err);
                }
            }

            Result::<()>::Ok(())
        });
        (result_receiver, published)
    }

    pub async fn start_audio_observer_task(&self) {
        let mut stop_receiver = self.stop_sender_channel.subscribe();
        let id_out = self.id.clone();
        let observer = self.audio_observer.clone();
        let interval_ms = observer.lock().await.interval as u64;
        let interval = Duration::from_millis(interval_ms);
        let room_out = self.room.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sleep(interval) => {
                        let is_empty = {
                            observer.lock().await.is_empty().await
                        };

                        if is_empty {
                            info!("Continue");
                            continue;
                        }

                        let streams = {
                            observer.lock().await.calc().await
                        };

                        if let Some(streams) = streams {
                            info!("Streams {:?}", streams);
                            room_out.send_message(RoomEvent::VoiceActivity { room_id: room_out.id.clone(), stream_ids: streams }).await;
                        }
                    }
                    _ = stop_receiver.recv() => {
                        info!("[Router {}] Stopping audio observer task", id_out);
                        break;
                    }
                }
            }
        });
        info!("Audio observer task started");
    }

    pub async fn set_rtcp_writer(&self, writer: RtcpWriterFn) {
        let mut handler = self.rtcp_writer_handler.lock().await;
        *handler = Some(writer);
    }
    pub async fn send_rtcp(&self) {
        let mut stop = self.stop_sender_channel.subscribe();
        let mut rtcp_receiver = self.rtcp_receiver_channel.lock().await;
        loop {
            //let mut rtcp_receiver = self.rtcp_receiver_channel.lock().await;
            tokio::select! {
              data = rtcp_receiver.recv() => {
                 if let Some(val) = data{
                    if let Some(f) = &mut *self.rtcp_writer_handler.lock().await {
                        let _ = f(val).await;
                    }
                }
              }
              _data = stop.recv() => {
                info!("Stop receiver signal. Exiting loop");
                return ;
              }
            };
        }
    }
    pub async fn stop(&self) {
        if let Err(err) = self.stop_sender_channel.send(()) {
            error!("stop err: {}", err);
        }
    }
}
