//! We implement perfect negotiation in order to try to
//! reduce the number of errors that can occur when streams
//! and conditions change. In this case, the server is
//! considered the impolite peer.
//!
//! https://w3c.github.io/webrtc-pc/#perfect-negotiation-example

use std::
    sync::atomic::{AtomicBool, Ordering}
;

use anyhow::Result;
use webrtc::{
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::{
        sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
        signaling_state::RTCSignalingState,
    },
};


use super::Peer;

/// Current negotiation state
#[derive(Default)]
pub struct NegotiationState {
    making_offer: AtomicBool,
    ignore_offer: AtomicBool,
    is_setting_remote_answer_pending: AtomicBool,
}

impl Peer {
    /// Renegotiate the current connection
    pub async fn renegotiate(&self) -> Result<()> {
        // TODO: not sure if required
        // ignore for now

        /* // Signal that we are currently creating an offer
        self.negotiation_state
            .making_offer
            .store(true, Ordering::SeqCst);

        // Create an offer
        let offer = self.connection.create_offer(None).await?;

        // Set the local description based on this offer
        self.connection.set_local_description(offer.clone()).await?;

        // Send an answer back
        (self.negotation_fn)(Negotiation::SDP { description: offer }).await?;

        // Mark as complete
        self.negotiation_state
            .making_offer
            .store(false, Ordering::SeqCst); */

        Ok(())
    }


    /// Consume a given RTC session description
    pub async fn consume_sdp(&self, description: RTCSessionDescription) -> Result<()> {
        // Check if we are ready to receive a new SDP
        let ready_for_offer = !self.negotiation_state.making_offer.load(Ordering::SeqCst)
            && (self.pc.signaling_state() == RTCSignalingState::Stable
                || self
                    .negotiation_state
                    .is_setting_remote_answer_pending
                    .load(Ordering::SeqCst));

        // Check if this offer is unexpected
        let sdp_type = description.sdp_type.clone();
        let offer_collision = sdp_type == RTCSdpType::Offer && !ready_for_offer;

        // We are the impolite peer hence we ignore the offer
        self.negotiation_state
            .ignore_offer
            .store(offer_collision, Ordering::Relaxed);

        if offer_collision {
            warn!("Unexpected offer received from the client!");
            return Ok(());
        }

        // If we received an answer, mark it as such locally
        self.negotiation_state
            .is_setting_remote_answer_pending
            .store(sdp_type == RTCSdpType::Answer, Ordering::SeqCst);

        // Apply the new remote description
        self.pc.set_remote_description(description).await?;

        // Restore the default value
        self.negotiation_state
            .is_setting_remote_answer_pending
            .store(false, Ordering::SeqCst);

        // If we received an offer, send an answer back
        if sdp_type == RTCSdpType::Offer {
            // Create an answer
            let answer = self.pc.create_answer(None).await?;

            // Set the local description based on this answer
            self.pc
                .set_local_description(answer.clone())
                .await?;

            // Send answer back to all clients
            //self.room.publish(RoomEvent::CreateAnswer(Negotiation::SDP { description: answer, media_type_buffer: None }));
        }

        Ok(())
    }

    /// Consume a given ICE candidate
    pub async fn consume_ice(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        if let Err(error) = self.pc.add_ice_candidate(candidate).await {
            if !self.negotiation_state.ignore_offer.load(Ordering::SeqCst) {
                return Err(error.into());
            }
        }

        Ok(())
    }

    pub async fn create_offer(&self) -> Result<RTCSessionDescription> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer)
    }
}