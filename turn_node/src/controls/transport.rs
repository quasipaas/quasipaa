use std::str::from_utf8 as str_from_utf8;
use num_enum::TryFromPrimitive;
use serde_json as Json;
use anyhow::{
    Result,
    Error,
    anyhow
};

use std::{
    collections::HashMap,
    convert::TryFrom,
    future::Future,
    sync::Arc
};

use serde::{
    de::DeserializeOwned,
    Serialize
};

use tokio::{
    net::TcpStream,
    sync::RwLock
};

use tokio::net::tcp::{
    OwnedReadHalf,
    OwnedWriteHalf
};

use tokio::sync::oneshot::{
    channel,
    Sender,
};

use tokio::sync::mpsc::{
    unbounded_channel,
    UnboundedSender,
};

use tokio::io::{
    AsyncReadExt,
    AsyncWriteExt
};

use bytes::{
    BytesMut,
    BufMut,
    Bytes,
    Buf
};

/// 负载类型
///
/// * `Request` 请求
/// * `Reply` 正确响应
/// * `Error` 错误响应
#[repr(u8)]
#[derive(PartialEq, Eq)]
#[derive(TryFromPrimitive)]
enum Flag {
    Request = 0,
    Reply = 1,
    Error = 2
}

/// 请求ID
#[derive(Default)]
struct Uid {
    inner: u32
}

/// 缓冲区
#[derive(Default)]
struct Buffer {
    inner: BytesMut
}

/// RPC传输
///
/// * `call_stack` 回调栈表
/// * `listener` 监听器表
/// * `inner` TCP连接
/// * `buffer` 缓冲区
/// * `uid` 内部ID偏移量
pub struct Transport {
    call_stack: RwLock<HashMap<u32, Sender<Result<Bytes, Error>>>>,
    listener: RwLock<HashMap<u8, UnboundedSender<(u32, Bytes)>>>,
    inner_writer: RwLock<OwnedWriteHalf>,
    inner_reader: RwLock<OwnedReadHalf>,
    buffer: RwLock<Buffer>,
    uid: RwLock<Uid>,
}

impl Transport {
    /// 创建实例
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use super::Transport;
    /// 
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let socket = TcpStream::connect(addr).await?;
    /// let transport = Transport::new(socket);
    /// transport.run();
    /// ```
    pub fn new(socket: TcpStream) -> Arc<Self> {
        let (reader, writer) = socket.into_split();
        Arc::new(Self {
            call_stack: RwLock::new(HashMap::new()),
            buffer: RwLock::new(Buffer::default()),
            listener: RwLock::new(HashMap::new()),
            inner_reader: RwLock::new(reader),
            inner_writer: RwLock::new(writer),
            uid: RwLock::new(Uid::default()),
        })
    }
    
    /// 启动
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use super::Transport;
    /// 
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let socket = TcpStream::connect(addr).await?;
    /// let transport = Transport::new(socket);
    /// transport.run();
    /// ```
    #[rustfmt::skip]
    pub fn run(self: Arc<Self>) -> Arc<Self> {
        let s = self.clone();
        tokio::spawn(async move {
            loop { let _ = s.poll().await; }
        });

        self
    }

    /// 绑定事件处理器
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use super::Transport;
    /// 
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let socket = TcpStream::connect(addr).await?;
    /// let transport = Transport::new(socket);
    /// transport.run();
    ///
    /// transport.bind(0, |req: String| async move {
    ///     Ok("panda")
    /// }).await;
    /// ```
    #[rustfmt::skip]
    pub async fn bind<T, F, D, U>(self: Arc<Self>, kind: u8, mut handler: T)
    where
        D: Serialize + Send,
        U: DeserializeOwned + Send,
        F: Future<Output = Result<D, Error>> + Send,
        T: FnMut(U) -> F + Send + 'static
    {
        let (writer, mut reader) = unbounded_channel();
        self.listener.write().await.insert(kind, writer);

    tokio::spawn(async move {loop {
        let (id, buf) = match reader.recv().await {
            None => continue,
            Some(m) => m
        };

        let result = match Json::from_slice(&buf[..]) {
            Ok(q) => (handler)(q).await,
            Err(_) => continue
        };

        if let Err(e) = self.listen_hook(kind, id, result).await {
            log::error!("transport err: {:?}", e);
        }
    }});
        
    }

    /// 呼叫远端
    ///
    /// # Example
    ///
    /// ```no_run
    /// use tokio::net::TcpStream;
    /// use super::Transport;
    /// 
    /// let addr = "127.0.0.1:8080".parse()?;
    /// let socket = TcpStream::connect(addr).await?;
    /// let transport = Transport::new(socket);
    /// transport.run();
    ///
    /// let name = transport.call(0, "username").await?;
    /// ```
    #[rustfmt::skip]
    pub async fn call<T, U>(&self, kind: u8, data: &T) -> Result<U>
    where
        T: Serialize,
        U: DeserializeOwned
    {
        let mut uid = self.uid.write().await;
        uid.inner = if uid.inner >= u32::MAX { 0 } else { uid.inner + 1 };

        let (writer, reader) = channel();
        self.call_stack.write().await.insert(uid.inner, writer);

        let req_buf = Json::to_vec(data)?;
        self.send(kind, Flag::Request, uid.inner, &req_buf).await?;

        let buf = reader.await??;
        let reply = Json::from_slice(&buf)?;
        Ok(reply)
    }

    /// 发送消息到远端
    ///
    /// 将消息打包之后分段推送到Socket
    /// 分段提交之后flush到对端，期望达到整段到达的效果
    async fn send(&self, kind: u8, flag: Flag, id: u32, buf: &[u8]) -> Result<()> {
        let mut header = BytesMut::new();
        let mut socket = self.inner_writer.write().await;

        header.put_u32(buf.len() as u32);
        header.put_u8(kind);
        header.put_u8(flag as u8);
        header.put_u32(id);

        socket.write_all(&header).await?;
        socket.write_all(&buf).await?;
        socket.flush().await?;

        Ok(())
    }
    
    /// 事件处理程序返回处理
    ///
    /// 根据返回的Result，序列化成对应消息
    /// 并发送到对端，错误直接发送字符串
    #[rustfmt::skip]
    async fn listen_hook<T>(&self, kind: u8, id: u32, result: Result<T>) -> Result<()>
    where T : Serialize
    {
        let flag = match result {
            Ok(_) => Flag::Reply,
            Err(_) => Flag::Error,
        };

        let body = match result {
            Ok(r) => Json::to_string(&r)?,
            Err(e) => e.to_string(),
        };

        self.send(
            kind,
            flag,
            id,
            body.as_bytes()
        ).await
    }

    /// 内部循环
    ///
    /// 从Socket读入到内部缓冲区暂存，并尽量解码出消息，
    /// 直到无法继续处理，收缩内部缓冲区
    #[rustfmt::skip]
    async fn poll(&self) -> Result<()> {
        let mut buf = self.buffer.write().await;
        self.inner_reader.write().await.read_buf(&mut buf.inner).await?;

    loop {
        
        // 检查缓冲区长度是否满足基本要求
        // 如果不满足则跳出循环
        if buf.inner.len() <= 10 {
            break;
        }

        // 获取消息长度
        let size = u32::from_be_bytes([
            buf.inner[0],
            buf.inner[1],
            buf.inner[2],
            buf.inner[3]
        ]) as usize;
        
        // 检查缓冲区长度，确认消息是否完全到达
        if size + 10 > buf.inner.len() {
            break;
        }

        // 因为获取长度为窥视并不消耗
        // 所以这里手动消耗掉u32
        buf.inner.advance(4);

        // 获取消息事件
        // 获取消息类型
        // 获取消息ID
        // 获取消息内容
        let kind = buf.inner.get_u8();
        let flag = Flag::try_from(buf.inner.get_u8())?;
        let id = buf.inner.get_u32();
        let body = buf.inner.split_to(size).freeze();

        // 根据不同消息类型
        // 交给对应处理程序
        let _ = match flag {
            Flag::Request => self.process_request(kind, id, body).await,
            Flag::Reply => self.process_reply(id, body).await,
            Flag::Error => self.process_error(id, body).await
        };
    }

        Ok(())
    }
    
    #[rustfmt::skip]
    async fn process_request(&self, kind: u8, id: u32, body: Bytes) -> Option<()> {
        let mut listener = self.listener.write().await;
        listener.get_mut(&kind)?.send((id, body)).unwrap();
        None
    }

    #[rustfmt::skip]
    async fn process_reply(&self, id: u32, body: Bytes) -> Option<()> {
        let mut call = self.call_stack.write().await;
        call.remove(&id)?.send(Ok(body)).unwrap();
        None
    }
    
    #[rustfmt::skip]
    async fn process_error(&self, id: u32, body: Bytes) -> Option<()> {
        let e = anyhow!(str_from_utf8(&body[..]).ok()?.to_string());
        let mut call = self.call_stack.write().await;
        call.remove(&id)?.send(Err(e)).unwrap();
        None
    }
}