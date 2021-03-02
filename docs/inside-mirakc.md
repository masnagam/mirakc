# Inside mirakc

## Broadcaster

The `Broadcaster` is one of key types in a mechanism to share a tuner device
with multiple subscribers.

The `Broadcaster` manages a MPEG-TS streaming from a tuner device and accepts a
subscription from a subscriber who likes to receive the MPEG-TS streaming.  When
a data chunk arrives, the `Broadcaster` delivers it to subscribers.

Data chunks are delivered through a `tokio::sync::mpsc::channel` between the
`Broadcaster` and each subscriber.  The `tokio::sync::mpsc::channel` is also
used as a buffer for data chunks.

```
tuner-device
  |
  V
Broadcaster
  |
  +--> tokio::sync::mpsc::channel --> subscriber-1
  |
  +--> tokio::sync::mpsc::channel --> subscriber-2
```

## Streaming Pipeline

mirakc has a pipeline to process MPEG-TS packets.

```
+------------ CommandPipeline ---------------+
| tuner-command (external process)           |
|   |                                        |
|   V                                        |
| tuner-filter (external process) [optional] |
+---|----------------------------------------+
    |
+---V------ Broadcaster -----------------+
| tokio::sync::mpsc::channel (as buffer) |
+---|------------------------------------+
    V
  MpegTsStream
    |
    +--(when using filters)
    |      |
    |  +---V-- CommandPipeline (Filter Pipeline) -----+
    |  | pre-filters (external process) [optional]    |
    |  |   |                                          |
    |  |   V                                          |
    |  | decode-filter (external process) [optional]  |
    |  |   |                                          |
    |  |   V                                          |
    |  | service-filter (external process) [optional] |
    |  |   or                                         |
    |  | program-filter (external process) [optional] |
    |  |   |                                          |
    |  |   V                                          |
    |  | post-filters (external process) [optional]   |
    |  +---|------------------------------------------+
    |      V
    |    tokio::sync::mpsc::channel (as buffer) <web-buffer>
    |      |
    +------+
    |
    V
  actix-web (HTTP chunk encoding)
    |
    V
  client
```

Writing data to the input-side endpoint of the `CommandPipeline` and reading
data from the output-side endpoint of the `CommandPipeline` are processed by
individual asynchronous tasks.

Data transfer between adjacent processes in the `CommandPipeline` is performed
through a UNIX pipe.  And data is copied synchronously.  This means that writing
to `stdout` in the upstream process is blocked when the buffer of the UNIX pipe
is full.

The `web-buffer` can be tuned by the following configuration properties:

* [server.stream-chunk-size](./config.md#serverstream-chunk-size)
* [server.stream-max-chunks](./config.md#serverstream-max-chunks)

See also the Japanese discussion on
[issues/18](https://github.com/mirakc/mirakc/issues/18).

## Timeshift Recording

The timeshift recording consists of the following pipeline.

```
+------------ CommandPipeline ----------------+
| tuner-command (external process)            |
|   |                                         |
|   V                                         |
| tuner-filter (external process) [optional]  |
+---|-----------------------------------------+
    |
+---V------ Broadcaster -----------------+
| tokio::sync::mpsc::channel (as buffer) |
+---|------------------------------------+
    V
  MpegTsStream
    |
+---V--------- CommandPipeline -------+
| service-filter (external process)   |
|   |                                 |
|   V                                 |
| decode-filter (external process)    |
|   |                                 |
|   V                                 |
| service-recorder (external process) |
|   | |                               |
|   | |<TS Packets>                   |
|   | |                               |
|   | +--> config.timeshift[].file    |
|   |      (fixed-size ring buffer)   |
+---|---------------------------------+
    |
    |<JSON Messages>
    |
    V
  TimeshiftRecorder (Actor)
```

The `service-recorder` command writes filtered TS packets into the timeshift
record file and outputs JSON messages to STDOUT.

The `TimeshiftRecorder` actor receives the JSON messages from the
`service-recorder` command, and update internal information about records of
TV programs in the timeshift file.

The timeshift file is divided into chunks whose size is specified with
`config.timeshift[].chunk-size`.  The maximum number of chunks is specified with
`config.timeshift[].num-chunks`.  Therefore, the maximum size of the timeshift
file is fixed.

```
Timeshift File (Max Size = chunk-size * num-chunks)
+------------------------------------------------------------------------------+
| Chunk#0 | Chunk#1 | ...                             | Chunk#<num-chunks - 1> |
+------------------------------------------------------------------------------+
```

It's recommended to create the timeshift file with the maximum size before
starting mirakc if you like to avoid write errors due to insufficient disk space:

```shell
fallocate -l $(expr <chunk-size> \* <num-chunks>) /path/to/timeshift.m2ts
```

A buffer used inside the system library is flushed when the file position
reaches the boundary between the current chunk and the next chunk.

The `TimeshiftRecorder` actor manages the chunks based on JSOM messages from the
`service-recorder` command.  A chunk currently written and following
`config.timeshift[].num-reserves` chunks are never supplied for streaming.

```
Timeshift File
+------------------------------------------------------------------------------+
| Chunk#0 | Chunk#1 | Chunk#2 | ... | Chunk#<2 + num-reserves> |   Chunks...   |
|      (ready)      |   A     |         (reserve)              |    (ready)    |
+-----------------------|------------------------------------------------------+
                     <File Position>
```
