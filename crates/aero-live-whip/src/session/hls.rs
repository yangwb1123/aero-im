//! Cancellation-safe WHIP-to-HLS task composition.

use tokio::net::UdpSocket;

use super::{SessionError, WhipSession};

impl WhipSession {
    /// Drive this session straight into HLS on disk under `stream_dir`.
    ///
    /// The sink performs H.264-to-MPEG-TS conversion while the paired async
    /// writer persists `stream_dir/{segment}.ts` and `index.m3u8`.
    pub async fn run_to_hls(
        self,
        socket: UdpSocket,
        stream_dir: std::path::PathBuf,
        target_duration_secs: u32,
    ) -> Result<u64, SessionError> {
        Box::pin(self.run_to_hls_until_cancelled(
            socket,
            stream_dir,
            target_duration_secs,
            std::future::pending(),
        ))
        .await
    }

    /// Drive this session into HLS until WebRTC ends or `cancelled` resolves.
    ///
    /// Cancelling through this API always drops the media sink and awaits the
    /// HLS writer. The writer can therefore flush the trailing GOP and finalize
    /// its manifest instead of surviving as a detached task.
    pub async fn run_to_hls_until_cancelled(
        self,
        socket: UdpSocket,
        stream_dir: std::path::PathBuf,
        target_duration_secs: u32,
        cancelled: impl std::future::Future<Output = ()> + Send,
    ) -> Result<u64, SessionError> {
        let (sink, writer) = crate::hls_sink::hls_sink(stream_dir, target_duration_secs)
            .await
            .map_err(|error| SessionError::Io(std::io::Error::other(error.to_string())))?;
        let writer_task = tokio::spawn(writer.run());
        let session_result = {
            // Whichever branch wins, dropping `run` here drops its owned sink
            // before the writer is awaited, closing the segment channel.
            let run = self.run(socket, sink);
            tokio::pin!(run);
            tokio::pin!(cancelled);
            tokio::select! {
                result = &mut run => result,
                () = &mut cancelled => Ok(()),
            }
        };
        let writer_result = match writer_task.await {
            Ok(Ok(segments)) => Ok(segments),
            Ok(Err(error)) => Err(SessionError::Io(std::io::Error::other(error.to_string()))),
            Err(join_error) => Err(SessionError::Io(std::io::Error::other(
                join_error.to_string(),
            ))),
        };
        session_result?;
        writer_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_awaits_writer_and_finalizes_empty_manifest() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_addr = socket.local_addr().unwrap();
        let (session, _) = WhipSession::accept(
            super::super::tests::PUBLISHER_OFFER,
            "127.0.0.1",
            local_addr.port(),
        )
        .unwrap();
        let temp = crate::testutil::TempDir::new().unwrap();
        let stream_dir = temp.path().join("cancelled");

        let segments = Box::pin(tokio::time::timeout(
            std::time::Duration::from_secs(2),
            session.run_to_hls_until_cancelled(
                socket,
                stream_dir.clone(),
                2,
                std::future::ready(()),
            ),
        ))
        .await
        .expect("cancellation must not leave the HLS writer detached")
        .unwrap();

        assert_eq!(segments, 0);
        let manifest = std::fs::read_to_string(stream_dir.join("index.m3u8")).unwrap();
        assert!(manifest.contains("#EXT-X-ENDLIST"));
    }
}
