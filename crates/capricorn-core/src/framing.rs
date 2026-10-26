use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

pub const MAX_FRAME: u32 = 128 * 1024 * 1024;

/// The encoded message lives in a buffer of exactly its size that is wiped afterwards,
/// since requests and replies may carry passwords or recovery codes.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let size = postcard::experimental::serialized_size(msg).map_err(invalid_data)?;
    let len = u32::try_from(size)
        .ok()
        .filter(|&len| len <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Zeroizing::new(Vec::with_capacity(4 + size));
    frame.extend_from_slice(&len.to_be_bytes());
    postcard::to_io(msg, &mut *frame).map_err(invalid_data)?;
    w.write_all(&frame).await?;
    w.flush().await
}

pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    read_msg_limited(r, MAX_FRAME).await
}

/// Rejects a length prefix above `max` before reading the body. The body goes into a
/// buffer reserved at exactly its length, so it never regrows and leaves no unwiped
/// copies, and the buffer is wiped once decoded.
pub async fn read_msg_limited<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    max: u32,
) -> io::Result<T> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix).await?;
    let len = u32::from_be_bytes(prefix);
    if len > max.min(MAX_FRAME) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds limit"),
        ));
    }
    let mut body = Zeroizing::new(Vec::with_capacity(len as usize));
    let read = r.take(u64::from(len)).read_to_end(&mut body).await?;
    if read != len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    postcard::from_bytes(&body).map_err(invalid_data)
}

fn invalid_data(err: postcard::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ClientReq, SearchQuery};
    use tokio::io::duplex;

    fn query() -> ClientReq {
        ClientReq::Search(SearchQuery {
            text: "hello".into(),
            kind: None,
            app: Some("firefox".into()),
            workspace: None,
            from: Some(1),
            to: None,
            limit: 20,
        })
    }

    #[tokio::test]
    async fn framing_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        write_msg(&mut a, &query()).await.unwrap();
        let got: ClientReq = read_msg(&mut b).await.unwrap();
        assert_eq!(got, query());
    }

    #[tokio::test]
    async fn framing_rejects_oversize() {
        let (mut a, mut b) = duplex(1024);
        a.write_all(&(MAX_FRAME + 1).to_be_bytes()).await.unwrap();
        let err = read_msg::<_, ClientReq>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn framing_rejects_four_gib_prefix() {
        let (mut a, mut b) = duplex(1024);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let err = read_msg::<_, ClientReq>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn framing_garbage_is_error() {
        let (mut a, mut b) = duplex(1024);
        let junk = [0xffu8; 16];
        a.write_all(&(junk.len() as u32).to_be_bytes())
            .await
            .unwrap();
        a.write_all(&junk).await.unwrap();
        let err = read_msg::<_, ClientReq>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn limited_read_rejects_frames_above_its_limit() {
        let (mut a, mut b) = duplex(4096);
        write_msg(&mut a, &query()).await.unwrap();
        let err = read_msg_limited::<_, ClientReq>(&mut b, 8)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let (mut a, mut b) = duplex(4096);
        write_msg(&mut a, &query()).await.unwrap();
        let got: ClientReq = read_msg_limited(&mut b, 1024).await.unwrap();
        assert_eq!(got, query());
    }

    #[tokio::test]
    async fn write_msg_frames_exactly() {
        let mut out = Vec::new();
        write_msg(&mut out, &query()).await.unwrap();
        let body = postcard::to_stdvec(&query()).unwrap();
        assert_eq!(&out[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&out[4..], &body[..]);
    }

    #[tokio::test]
    async fn framing_truncated_body_is_eof() {
        let (mut a, mut b) = duplex(1024);
        a.write_all(&100u32.to_be_bytes()).await.unwrap();
        a.write_all(&[1, 2, 3]).await.unwrap();
        drop(a);
        let err = read_msg::<_, ClientReq>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
