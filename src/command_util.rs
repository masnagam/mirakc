use std::env;
use std::fmt;
use std::io;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::process::{Command, Child, ChildStdin, ChildStdout, Stdio};
use std::task::{Poll, Context};

use failure::Fail;
use tokio::prelude::*;
use tokio::io::BufReader;
use tokio::sync::broadcast;

use crate::tokio_snippet;

pub fn spawn_process(
    command: &str,
    input: Stdio,
) -> Result<Child, Error> {
    let words = match shell_words::split(command) {
        Ok(words) => words,
        Err(_) => return Err(Error::UnableToParse(command.to_string())),
    };
    let words: Vec<&str> = words.iter().map(|word| &word[..]).collect();
    let (prog, args) = words.split_first().unwrap();
    let debug_child_process =
        env::var_os("MIRAKC_DEBUG_CHILD_PROCESS").is_some();
    let stderr = if debug_child_process {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = Command::new(prog)
        .args(args)
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(|err| Error::UnableToSpawn(command.to_string(), err))?;
    if cfg!(not(test)) {
        if debug_child_process {
            let prog = prog.to_string();
            let child_id = child.id();
            let child_stderr = tokio_snippet::stdio(child.stderr.take())
                .map_err(|err| Error::AsyncIoRegistrationFailure(err))?.unwrap();
            tokio::spawn(async move {
                let mut reader = BufReader::new(child_stderr);
                let mut data = Vec::with_capacity(4096);
                // BufReader::read_line() gets stuck if an invalid character
                // sequence is found.  Therefore, we use BufReader::read_until()
                // here in order to avoid such a situation.
                while let Ok(n) = reader.read_until(0x0A, &mut data).await {
                    if n == 0 {  // EOF
                        break;
                    }
                    log::debug!("{}#{}: {}", prog, child_id,
                                // data may contain non-utf8 sequence.
                                String::from_utf8_lossy(&data).trim_end());
                    data.clear();
                }
            });
        }
    }
    Ok(child)
}

// Spawn processes for input commands and build a pipeline, then returns it.
// Input and output endpoints can be took from the pipeline only once
// respectively.
pub fn spawn_pipeline<T>(
    commands: Vec<String>,
    id: T,
) -> Result<CommandPipeline<T>, Error>
where
    T: fmt::Display + Clone + Unpin,
{
    let mut pipeline = CommandPipeline::new(id);
    for command in commands.into_iter() {
        pipeline.spawn(command)?;
    }
    Ok(pipeline)
}

// errors

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display = "Unable to parse: {}", 0)]
    UnableToParse(String),
    #[fail(display = "Unable to spawn: {}: {}", 0, 1)]
    UnableToSpawn(String, io::Error),
    #[fail(display = "Async I/O registration failure: {}", 0)]
    AsyncIoRegistrationFailure(io::Error),
}

// pipeline builder

pub struct CommandPipeline<T>
where
    T: fmt::Display + Clone + Unpin,
{
    id: T,
    sender: broadcast::Sender<()>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    commands: Vec<CommandData>,
}

struct CommandData {
    command: String,
    process: Child,
}

impl<T> CommandPipeline<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn new(id: T) -> Self {
        let (sender, _) = broadcast::channel(1);
        Self {
            id,
            sender,
            stdin: None,
            stdout: None,
            commands: Vec::new(),
        }
    }

    fn spawn(&mut self, command: String) -> Result<(), Error> {
        let input = if self.stdout.is_none() {
            Stdio::piped()
        } else {
            Stdio::from(self.stdout.take().unwrap())
        };

        let mut process = spawn_process(&command, input)?;
        log::debug!("{}: Spawned {}: `{}`",
                    self.id, process.id(), command);

        if self.stdin.is_none() {
            self.stdin = process.stdin.take();
        }
        self.stdout = process.stdout.take();
        self.commands.push(CommandData { command, process });

        Ok(())
    }

    pub fn get_model(&self) -> Vec<CommandPipelineProcessModel> {
        self.commands
            .iter()
            .map(|data| CommandPipelineProcessModel {
                command: data.command.clone(),
                pid: data.process.id(),
            })
            .collect()
    }

    pub fn pids(&self) -> Vec<u32> {
        self.commands
            .iter()
            .map(|data| data.process.id())
            .collect()
    }

    pub fn take_endpoints(
        &mut self
    ) -> Result<(CommandPipelineInput<T>, CommandPipelineOutput<T>), Error> {
        assert!(self.stdin.is_some());
        assert!(self.stdout.is_some());
        let input = CommandPipelineInput::new(
            Self::make_childio_async(self.stdin.take())?, self.id.clone(),
            self.sender.subscribe());
        let output = CommandPipelineOutput::new(
            Self::make_childio_async(self.stdout.take())?, self.id.clone(),
            self.sender.subscribe());
        Ok((input, output))
    }

    fn make_childio_async<F: AsRawFd>(
        childio: Option<F>
    ) -> Result<tokio_snippet::ChildIo<F>, Error> {
        match tokio_snippet::stdio(childio) {
            Ok(childio) => Ok(childio.expect("Should not be None")),
            Err(err) => Err(Error::AsyncIoRegistrationFailure(err)),
        }
    }
}

impl<T> Drop for CommandPipeline<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn drop(&mut self) {
        // Always kill the processes and ignore the error.  Because there is no
        // method to check whether the process is alive or dead.
        for data in self.commands.iter_mut() {
            let _ = data.process.kill();
            let _ = data.process.wait();
            log::debug!("{}: Killed {}: `{}`",
                        self.id, data.process.id(), data.command);
        }
    }
}

pub struct CommandPipelineProcessModel {
    pub command: String,
    pub pid: u32,
}

// input-side endpoint

pub struct CommandPipelineInput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    inner: tokio_snippet::ChildIo<ChildStdin>,
    _pipeline_id: T,
    receiver: broadcast::Receiver<()>,
}

impl<T> CommandPipelineInput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn new(
        inner: tokio_snippet::ChildIo<ChildStdin>,
        pipeline_id: T,
        receiver: broadcast::Receiver<()>,
    ) -> Self {
        Self { inner, _pipeline_id: pipeline_id, receiver }
    }

    fn has_pipeline_broken(&mut self) -> bool {
        match self.receiver.try_recv() {
            Err(err) if err == broadcast::TryRecvError::Empty => false,
            _ => true,
        }
    }
}

impl<T> AsyncWrite for CommandPipelineInput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8]
    ) -> Poll<io::Result<usize>> {
        if self.has_pipeline_broken() {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "")))
        } else {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context
    ) -> Poll<io::Result<()>> {
        if self.has_pipeline_broken() {
            Poll::Ready(Ok(()))
        } else {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context
    ) -> Poll<io::Result<()>> {
        if self.has_pipeline_broken() {
            Poll::Ready(Ok(()))
        } else {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }
}

// output-side endpoint

pub struct CommandPipelineOutput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    inner: tokio_snippet::ChildIo<ChildStdout>,
    _pipeline_id: T,
    receiver: broadcast::Receiver<()>,
}

impl<T> CommandPipelineOutput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn new(
        inner: tokio_snippet::ChildIo<ChildStdout>,
        pipeline_id: T,
        receiver: broadcast::Receiver<()>,
    ) -> Self {
        Self { inner, _pipeline_id: pipeline_id, receiver }
    }

    fn has_pipeline_broken(&mut self) -> bool {
        match self.receiver.try_recv() {
            Err(err) if err == broadcast::TryRecvError::Empty => false,
            _ => true,
        }
    }
}

impl<T> AsyncRead for CommandPipelineOutput<T>
where
    T: fmt::Display + Clone + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8]
    ) -> Poll<io::Result<usize>> {
        if self.has_pipeline_broken() {
            Poll::Ready(Ok(0))  // EOF
        } else {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::*;

    #[test]
    fn test_spawn_process() {
        let result = spawn_process("sh -c 'exit 0;'", Stdio::null());
        assert!(result.is_ok());
        let _ = result.unwrap().wait();

        let result = spawn_process("'", Stdio::null());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Unable to parse: '");

        let result = spawn_process("command-not-found", Stdio::null());
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(),
                        Error::UnableToSpawn(_, io::Error {..}));
    }

    #[tokio::test]
    async fn test_pipeline() {
        use futures::task::noop_waker;

        let mut pipeline = spawn_pipeline(vec!["cat".to_string()], 0).unwrap();
        let (mut input, mut output) = pipeline.take_endpoints().unwrap();

        let result = input.write_all(b"hello").await;
        assert!(result.is_ok());

        let result = input.flush().await;
        assert!(result.is_ok());

        let result = input.shutdown().await;
        assert!(result.is_ok());

        let mut buf = [0; 6];

        let result = output.read(&mut buf).await;
        assert!(result.is_ok());
        assert_eq!(5, result.unwrap());
        assert_eq!(b"hello\0", &buf);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let poll =  Pin::new(&mut output).poll_read(&mut cx, &mut buf);
        assert!(poll.is_pending());

        drop(input);

        let result = output.read(&mut buf).await;
        assert!(result.is_ok());
        assert_eq!(0, result.unwrap());
    }

    #[tokio::test]
    async fn test_pipeline_input_dropped() {
        let mut pipeline = spawn_pipeline(vec!["cat".to_string()], 0).unwrap();
        let (mut input, mut output) = pipeline.take_endpoints().unwrap();

        let _ = input.write_all(b"hello").await;

        drop(input);

        let mut buf = [0; 5];

        let result = output.read(&mut buf).await;
        assert!(result.is_ok());
        assert_eq!(5, result.unwrap());
        assert_eq!(b"hello", &buf);
    }

    #[tokio::test]
    async fn test_pipeline_output_dropped() {
        let mut pipeline = spawn_pipeline(vec!["cat".to_string()], 0).unwrap();
        let (mut input, output) = pipeline.take_endpoints().unwrap();

        drop(output);

        let result = input.write_all(b"hello").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_pipeline_broken() {
        use tokio::sync::oneshot;

        struct Wrapper<W> {
            inner: W,
            sender: Option<oneshot::Sender<()>>,
        }

        impl<W> Wrapper<W> {
            fn new(inner: W, sender: Option<oneshot::Sender<()>>) -> Self {
                Wrapper { inner, sender }
            }
        }

        impl<W: AsyncWrite + Unpin> AsyncWrite for Wrapper<W> {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context,
                buf: &[u8]
            ) -> Poll<io::Result<usize>> {
                let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
                if poll.is_pending() {
                    if let Some(sender) = self.sender.take() {
                        let _ = sender.send(());
                    }
                }
                poll
            }

            fn poll_flush(
                mut self: Pin<&mut Self>,
                cx: &mut Context
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.inner).poll_flush(cx)
            }

            fn poll_shutdown(
                mut self: Pin<&mut Self>,
                cx: &mut Context
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.inner).poll_shutdown(cx)
            }
        }

        let mut pipeline = spawn_pipeline(vec![
            "sh -c 'exec 0<&-;'".to_string(),
        ], 0).unwrap();
        let (input, _) = pipeline.take_endpoints().unwrap();

        let (tx, rx) = oneshot::channel();
        let mut wrapper = Wrapper::new(input, Some(tx));

        let handle = tokio::spawn(async move {
            let data = [0; 8192];
            loop {
                if wrapper.write_all(&data).await.is_err() {
                    return;
                }
            }
        });

        // Wait until pending in `wrapper.write_all()`.
        let _ = rx.await;

        // Kill the spawned process while pending in `wrapper.write_all()`.
        drop(pipeline);

        // Will block until the task fails if the line above is removed.
        let result = handle.await;
        assert!(result.is_ok());
    }
}
