# Configuration

For simplicity, mirakc uses a single YAML file for configuration.

Most of properties in the configuration are optional.  You can simply omit
properties which have default values listed below if the default values are
suitable for your environment.

| PROPERTY                         | DEFAULT                                   |
|----------------------------------|-------------------------------------------|
| [epg.cache-dir]                  | `None`                                    |
| [server.addrs]                   | `[{http: 'localhost:40772'}]`             |
| [server.workers]                 | The number of CPUs                        |
| [server.stream-chunk-size]       | `32768` (32KiB)                           |
| [server.stream-max-chunks]       | `1000`                                    |
| [server.stream-time-limit]       | `16000` (16s)                             |
| [server.mounts]                  | `{}`                                      |
| [channels\[\].name]              |                                           |
| [channels\[\].type]              |                                           |
| [channels\[\].channel]           |                                           |
| [channels\[\].extra-args]        | `''`                                      |
| [channels\[\].services]          | `[]`                                      |
| [channels\[\].excluded-services] | `[]`                                      |
| [channels\[\].disabled]          | `false`                                   |
| [tuners\[\].name]                |                                           |
| [tuners\[\].types]               |                                           |
| [tuners\[\].command]             |                                           |
| [tuners\[\].time-limit]          | `30000` (30s)                             |
| [tuners\[\].disabled]            | `false`                                   |
| [tuners\[\].decoded]             | `false`                                   |
| [filters.tuner-filter.command]   | `''`                                      |
| [filters.service-filter.command] | `mirakc-arib filter-service --sid={{{sid}}}`|
| [filters.decode-filter.command]  | `''`                                      |
| [filters.program-filter.command] | `mirakc-arib filter-program --sid={{{sid}}} --eid={{{eid}}} --clock-pid={{{clock_pid}}} --clock-pcr={{{clock_pcr}}} --clock-time={{{clock_time}}} --end-margin=2000{{#video_tags}} --video-tag={{{.}}}{{/video_tags}}{{#audio_tags}} --audio-tag={{{.}}}{{/audio_tags}}` |
| [pre-filters]                    | `{}`                                      |
| [post-filters]                   | `{}`                                      |
| [jobs.scan-services.command]     | `mirakc-arib scan-services{{#sids}} --sids={{{.}}}{{/sids}}{{#xsids}} --xsids={{{.}}}{{/xsids}}` |
| [jobs.scan-services.schedule]    | `'0 31 5 * * * *'` (execute at 05:31 every day) |
| [jobs.sync-clocks.command]       | `mirakc-arib sync-clocks{{#sids}} --sids={{{.}}}{{/sids}}{{#xsids}} --xsids={{{.}}}{{/xsids}}` |
| [jobs.sync-clocks.schedule]      | `'0 3 12 * * * *'` (execute at 12:03 every day) |
| [jobs.update-schedules.command]  | `mirakc-arib collect-eits{{#sids}} --sids={{{.}}}{{/sids}}{{#xsids}} --xsids={{{.}}}{{/xsids}}` |
| [jobs.update-schedules.schedule] | `'0 7,37 * * * * *'` (execute at 7 and 37 minutes every hour) |
| [timeshift\[\].service-triple]   |                                           |
| [timeshift\[\].file]             |                                           |
| [timeshift\[\].chunk-size]       | 163840000 (4KiB * 40000 ~ 160MB)          |
| [timeshift\[\].num-chunks]       |                                           |
| [timeshift\[\].num-reserves]     | 1                                         |
| [timeshift\[\].priority]         | 128                                       |
| [resource.strings-yaml]          | `/etc/mirakc/strings.yml`                 |
| [mirakurun.openapi-json]         | `/etc/mirakurun.openapi.json`             |

[epg.cache-dir]: #epgcache-dir
[server.addrs]: #serveraddrs
[server.workers]: #serverworkers
[server.stream-chunk-size]: #serverstream-chunk-size
[server.stream-max-chunks]: #serverstream-max-chunks
[server.stream-time-limit]: #serverstream-time-limit
[server.mounts]: #servermounts
[channels\[\].name]: #channels
[channels\[\].type]: #channels
[channels\[\].channel]: #channels
[channels\[\].extra-args]: #channels
[channels\[\].services]: #channels
[channels\[\].excluded-services]: #channels
[channels\[\].disabled]: #channels
[tuners\[\].name]: #tuners
[tuners\[\].types]: #tuners
[tuners\[\].command]: #tuners
[tuners\[\].time-limit]: #tuners
[tuners\[\].disabled]: #tuners
[tuners\[\].decoded]: #tuners
[filters.tuner-filter.command]: #filterstuner-filter
[filters.service-filter.command]: #filtersservice-filter
[filters.decode-filter.command]: #filtersdecode-filter
[filters.program-filter.command]: #filtersprogram-filter
[pre-filters]: #pre-filters
[post-filters]: #post-filters
[jobs.scan-services.command]: #jobsscan-services
[jobs.scan-services.schedule]: #jobsscan-services
[jobs.sync-clocks.command]: #jobssync-clocks
[jobs.sync-clocks.schedule]: #jobssync-clocks
[jobs.update-schedules.command]: #jobsupdate-schedules
[jobs.update-schedules.schedule]: #jobsupdate-schdules
[timeshift\[\].service-triple]: #timeshift
[timeshift\[\].file]: #timeshift
[timeshift\[\].chunk-size]: #timeshift
[timeshift\[\].num-chunks]: #timeshift
[timeshift\[\].num-reserves]: #timeshift
[timeshift\[\].priority]: #timeshift
[resource.strings-yaml]: #resourcestrings-yaml
[mirakurun.openapi-json]: #mirakurunopenapi-json

## epg.cache-dir

An absolute path to a folder where EPG-related data will be stored.

`None` means that no data will be saved onto the filesystem.  In this case,
EPG-related data will be lost when mirakc stops.

```yaml
epg:
  cache-dir: /path/to/epg/cache
```

## server.addrs

`server.addrs` is a list of addresses to be bound.

There are two address types.

HTTP protocol:

```yaml
server:
  addrs:
    - http: '0.0.0.0:40772'
```

HTTPS protocol is not supported at this point.

UNIX domain socket:

```yaml
server:
  addrs:
    - unix: /var/run/mirakc.sock
```

mirakc never changes the ownership and permission of the socket.  Change them
after the socket has been created.  Or use SUID/SGID so that mirakc runs with
specific UID/GID.

Multiple addresses can be bound like below:

```yaml
server:
  addrs:
    - http: '0.0.0.0:40772'
    - unix: /var/run/mirakc.sock
```

## server.workers

The number of worker threads to serve the web API.

The specified number of threads are spawned and pooled at startup.

```yaml
server:
  workers: 2
```

## server.stream-chunk-size

The maximum size of a chunk for streaming.

```yaml
server:
  stream-chunk-size: 32768
```

An actual size of a chunk may be smaller than this value.

The default chunk size is 32 KiB which is large enough for 10 ms buffering.

## server.stream-max-chunks

The maximum number of chunks that can be buffered.

```yaml
server:
  stream-max-chunks: 1000
```

Chunks are dropped when the buffer is full.

The default maximum number of chunks is 1000 which is large enough for 10
seconds buffering if the chunk size is 32 KiB.

## server.stream-time-limit

The time limit for a streaming request.  The request will fail when the time
reached this limit.

```yaml
server:
  stream-time-limit: 20000
```

The value must be larger than `prepTime` defined in [EPGStation](https://github.com/l3tnun/EPGStation/blob/v1.6.9/src/server/Model/Operator/Recording/RecordingManageModel.ts#L45),
which is `15000` (15s) in v1.6.9.

This property is needed for avoiding the issue#1313 in actix-web in a streaming
request for a TV program.  In this case, no data is sent to the client until the
first TS packet comes from the streaming pipeline.  actix-web cannot detect the
client disconnect all that time due to the issue#1313.

## server.mounts

Definitions of mount points for static files and folders on the file system.

* path
  * An absolute path to a file or directory on the file system
* index (default: `None`)
  * A name of the index file
* listing (default: `false`)
  * Show entries listing for directories

```yaml
mounts:
  /file:
    path: /path/to/file
  /public:
    path: /path/to/public
    listing: true
  /:
    path: /path/to/html
    index: index.html
```

This property can be used for providing some kind of Web UI for mirakc.

## channels

Definitions of channels.  At least, one channel must be defined.

* name
  * An arbitrary name of the channel
* type
  * One of channel types in `GR`, `BS`, `CS` and `SKY`
* channel
  * A channel parameter used in a tuner command template
* extra-args
  * Extra arguments used in a tuner command template
* services
  * A list of SIDs (service identifiers) which must be included
  * An empty list means that all services found are included
* excluded-services
  * A list of SIDs which must be excluded
  * An empty list means that no service is excluded
  * Applied after processing the `services` property
* disabled (optional)
  * Disable the channel definition

```yaml
channels:
  - name: ETV
    type: GR
    channel: '26'

  # Disable NHK.
  - name: NHK
    type: GR
    channel: '27'
    disabled: true

  # Use only the service 101 in BS1.
  - name: BS1
    type: BS
    channel: BS15_0
    services: [101]

  # Exclude the service 531 from OUJ.
  - name: OUJ
    type: BS
    channel: BS11_2
    excluded-services: [531]

  # Extra arguments for szap-s2j
  - name: BS SPTV
    type: SKY
    channel: CH585
    extra-args: '-l JCSAT3A'
    serviceId: [33353]

  # Extra arguments for BonRecTest
  - name: ND02
    type: CS
    channel: '000'
    extra-args: '--space 1'
```

## tuners

Definitions of tuners.  At least, one tuner must be defined.

* name
  * An arbitrary name of the tuner
* types
  * A list of channel types supported by the tuner.
* command
  * A Mustache template string of a command to open the tuner
  * The command must output TS packets to `stdout`
* time-limit (optional)
  * A time limit in milliseconds
  * Stop streaming if no TS packet comes from the tuner for the time limit
* disabled (optional)
  * Disable the tuner
* decoded (optional)
  * PES packets are decoded by the tuner command

Command template variables:

* channel
  * The `channel` property of a channel defined in the `channels`
* channel_type
  * The `type` property of a channel defined in the `channels`
* duration
  * A duration to open the tuner in seconds
  * `-` means that the tuner is opened until the process terminates
  * TODO: `-` is always specified in the duration variable at this moment
* extra_args
  * The `extra-args` property of a channel defined in the `channels`

Cascading upstream Mirakurun-compatible servers is unsupported.  However, it's
possible to use upstream Mirakurun-compatible servers as tuners.  See the sample
below.

```yaml
tuners:
  - name: GR0
    types: [GR]
    command: >-
      recdvb {{{channel}}} {{{duration}}} -

  - name: Disabled
    types: [GR]
    command: >-
      cat /dev/null
    disabled: true

  # A tuner can be defined by using an "upstream" Mirakurun-compatible server.
  - name: upstream
    types: [GR, BS]
    command: >-
      curl -sG http://upstream:40772/api/channels/{{{channel_type}}}/{{{channel}}}/stream

```

## filters

Definitions of filters used in
[the streaming pipeline](./inside-mirakc.md#streaming-pipeline).

Each filter definition has the following properties:

* command
  * A Mustache template string of the filter command
  * The command must read data from `stdin`, and output the processed data to
    `stdout`
  * An empty string means that the filter is not defined
* content-type (optional)
  * A string of the content-type of data output from the filter
  * Absence of this property means that the filter doesn't change the
    content-type of the input data
  * Available only for the `post-filters`

Each Mustache template string defined in the `command` property will be rendered
with the following template data:

* tuner_index
  * The index of a tuner
  * Available only for the tuner-filter
* tuner_name
  * The `name` property of a tuner defined in the `tuners`
  * Available only for the tuner-filter
* channel_name
  * The `name` property of a channel defined in the `channels`
* channel_type
  * The `type` property of a channel defined in the `channels`
* channel
  * The `channel` property of a channel defined in the `channels`
* sid
  * The 16-bit integer identifier of a service (SID)
  * Available only for the service streaming, the program streaming and the record streaming
* eid
  * The 16-bit integer identifier of a program (EID)
  * Available only for the program streaming and the record streaming
* clock_pid
  * A PID of PCR packets to be used for the clock synchronization
  * Available only for the program streaming
* clock_pcr
  * A PCR value of synchronized clock for a service
  * Available only for the program streaming
* clock_time
  * A UNIX time (ms) of synchronized clock for a service
  * Available only for the program streaming
* video_tags
  * `component_tag` of a video stream in the program
  * Available only for the program streaming and the record streaming
* audio_tags
  * `component_tag`s of audio streams in the program
  * Available only for the program streaming and the record streaming
* id
  * The identifier of a record
  * Available only for the record streaming
* duration
  * A duration of a record in seconds
  * Available only for the record streaming
* size
  * Size of a record in bytes
  * Available only for the record streaming

### filters.tuner-filter

A filter which can be used for processing TS packets from a tuner command before
broadcasting the TS packets to subscribers.

This filter will be used not only for streaming API endpoints but also
background jobs if it's defined.

For example, this filter can be used for the drop-check for each tuner.

### filters.service-filter

A filter to drop TS packets which are not included in a specified service.

This filter will be used in the following streaming API endpoints:

* [/api/channels/{channel_type}/{channel}/services/{sid}/stream](./web-api.md#apichannelschannel_typechannelservicessidstream)
* [/api/services/{id}/stream](./web-api.md#apiservicesidstream)
* [/api/programs/{id}/stream](./web-api.md#apiprogramsidstream)

### filters.decode-filter

A filter to decode TS packets.

The `decode` query parameter for each streaming API endpoint configures the
decode-filter of the streaming.

### filters.program-filter

A filter to control streaming for a specified program.

This filter starts streaming when the program starts and stops streaming when
the program ends.

This filter will be used in the following streaming API endpoints:

* [/api/programs/{id}/stream](./web-api.md#apiprogramsidstream)

### pre-filters

A map of named filters which can be inserted at the input-side endpoint of the
filter pipeline.

The `pre-filters` query parameter for each streaming API endpoint configures the
pre-filters of the streaming.

The following request:

```
curl 'http://mirakc:40772/api/programs/{id}/stream?decode=1&pre-filters[0]=record'
```

will build the following filter pipeline:

```
pre-filters.record | service-filter | decode-filter | program-filter
```

### post-filters

A map of named filters which can be inserted at the output-side endpoint of the
filter pipeline.

The `post-filters` query parameter for each streaming API endpoint configures
the post-filters of the streaming.

The following request:

```
curl 'http://mirakc:40772/api/programs/{id}/stream?decode=1&post-filters[0]=transcode'
```

will build the following filter pipeline:

```
service-filter | decode-filter | program-filter | post-filters.transcode
```

## jobs

Definitions for background jobs.

Each job definition has the following properties:

* command
  * A Mustache template string of a command
* schedule
  * A crontab expression of the job schedule
  * See https://crates.io/crates/cron for details of the format

### jobs.scan-services

The scan-services job scans audio/video services in channels defined in the
`channels`.

The command must read TS packets from `stdin`, and output the result to `stdout`
in a specific JSON format.  See the help shown by `mirakc-arib scan-services -h`
for details of the JSON format.

Command template variables:

* sids
  * A list of SIDs which must be included
* xsids
  * A list of SIDs which must be excluded

### jobs.sync-clocks

The sync-clocks job synchronizes TDT/TOT and PRC value of each service.

The command must read TS packets from `stdin`, and output the result to `stdout`
in a specific JSON format.  See the help shown by `mirakc-arib sync-clocks -h`
for details of the JSON format.

Command template variables:

* sids
  * A list of SIDs which must be included
* xsids
  * A list of SIDs which must be excluded

### jobs.update-schedules

The update-schedules job updates EPG schedules for each service.

The command must read TS packets from `stdin`, and output the result to `stdout`
in a specific JSON format.  See the help shown by `mirakc-arib collect-eits -h`
for details of the JSON format.

Command template variables:

* sids
  * A list of SIDs which must be included
* xsids
  * A list of SIDs which must be excluded

## timeshift

Definitions of timeshift recordings.

* service-triple
  * A tuple of NID, TSID and SID of a service stream to record
* file
  * A path to a file used as a ring buffer to record the service stream
* chunk-size
  * Size of a data chunk
  * Must be a multiple of 8192
* num-chunks
  * The maximum number of chunks in the file
    * The maximum size of the file is computed by `chunk-size * num-chunks`
  * Must be larger than 2
* num-reserves
  * The number of chunks in the gap between the head and the tail of the ring buffer
  * Must be larger than 0
  * `num-chunks - num-reserves` must be larger than 1
* priority
  * The priority of streaming
  * Should be larger than 0

```yaml
timeshift:
  bs1:
    service-triple: [4, 16625, 101]  # BS1
    file: /path/to/bs1.timeshift.m2ts
    num-chunks: 4000  # about 640GB
```

The timeshift recording of mirakc is a similar function to the Timeshift Machine
implemented on TVs and recorders produced by Toshiba.  The timeshift recording
records TS packets in a service stream into a fixed-size file used as a 'ring'
buffer.  User can playback TV programs recorded in the file until they are
purged due to the file size limit.

The timeshift recording has the following limitations:

* All of recorded data is volatile
  * It will be empty when restarting
* Duration of a record currently being recorded is updated only when a chunk is
  filled or the TV program ends

## resource.strings-yaml

`resource.strings-yaml` specifies a path to a YAML file which contains strings
used in mirakc at runtime.

> TODO: This might be obsoleted by other tools like GNU gettext in the future.

## mirakurun.openapi-json

`mirakurun.openapi-json` specifies a path to an OpenAPI/Swagger JSON file
obtained from Mirakurun.

Applications including EPGStation use the Mirakurun client which uses `api/docs`
in order to query interfaces implemented by a web server that the client will
communicate.

```yaml
mirakurun:
  openapi-json: /path/to/mirakurun.openapi.json
```
