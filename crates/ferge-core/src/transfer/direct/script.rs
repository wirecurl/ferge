//! The POSIX shell scripts of a direct transfer job.
//!
//! A job lives in `~/.cache/ferge/jobs/<id>` on both servers. The *active*
//! side dials the other with `ssh` and runs the driver; the *passive* side
//! only answers through a forced command bound to the job's key. On push the
//! source is active, on pull the target.
//!
//! Active side: `driver.sh` (`probe` or `run`), `peer.sh` (how to reach the
//! other side), `lib.sh`, `start.sh`/`stop.sh`/`alive.sh`, `key`,
//! `known_hosts`, `list.<worker>` (what each worker copies), `batch/<n>.sh`,
//! and what the driver writes: `pid`, `status`, `log`, `err.*`.
//!
//! Passive side: `passive.sh` (the forced command), `lib.sh`, `tab/<n>.sh`
//! (file number → path), `batch/<n>.sh`, `authorized_key`, `install.sh`,
//! `uninstall.sh`.
//!
//! Both: `before/`, the state of the folders ferge adds to (see below). The
//! target side also has `stage`, naming its staging folder.
//!
//! # Copying
//!
//! Large files and resumed ones go one at a time: `put <file> <offset>`
//! (push: standard input becomes the file) or `get <file> <offset>` (pull:
//! the file is written to standard output). Each is written to
//! `<name>.ferge-part`, checked against its planned size, given its
//! modification time and only then renamed, without replacing an existing
//! file unless that was decided beforehand.
//!
//! Small files go in batches, one `tar` stream each (`putb <batch>` or
//! `getb <batch>`): the sender links the batch's files under their numbers
//! into a folder and archives that; the receiver unpacks it into the
//! staging folder `<target folder>/.ferge-<job>/<batch>`, checks every size
//! and moves each file into place the same way. A batch costs one login for
//! hundreds of files, and the archive carries the modification times.
//!
//! Paths never travel in requests: the passive side looks them up in its own
//! tables and batch files, so the key cannot touch any file outside the job.
//!
//! # Leaving servers as they were
//!
//! Before anything else, each side notes the state of the login directory,
//! `~/.cache`, `~/.cache/ferge`, `~/.cache/ferge/jobs`, `~/.ssh` and
//! `~/.ssh/authorized_keys`: whether each exists, what it holds (as a
//! checksum) and its times. Removing the job removes the folders ferge made,
//! the job's key line and what was added for it (a final newline, the file,
//! the `.ssh` folder), and gives each of those paths its earlier times back
//! if it holds again what it held. ferge then notes the state once more and
//! reports any difference.
//!
//! A job that ends without ferge tidies itself after a while, keeping only a
//! short result, which the next job on that server reports and removes
//! (`sweep`), along with keys of jobs that are gone or expired.
//!
//! # Status
//!
//! The driver appends one line per event to `status`:
//!
//! ```text
//! B <time> <pid>            job started
//! M ok|fail <time>          connection to the other side
//! s <file> <time>           file started (alone)
//! S <batch> <time>          batch started
//! d <file> <bytes> <time>   file done
//! e <file> <code> <time> <reason>
//! X <batch> <code> <time>   batch ended
//! E <code> <time>           job ended (143: stopped)
//! ```

use std::fmt::Write as _;

use crate::remote::shell::quote;

/// Folder of all jobs, relative to the login directory.
pub(crate) const JOBS_DIR: &str = ".cache/ferge/jobs";

/// Files per passive lookup table; a lookup reads one table.
pub(crate) const TABLE_SIZE: usize = 1000;

/// Files up to this size go in batches (when they start from the beginning).
pub(crate) const BATCH_FILE_LIMIT: u64 = 4 * 1024 * 1024;

/// Most bytes in one batch.
const BATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Fewest and most files in one batch.
const BATCH_FILES: (usize, usize) = (16, 512);

/// Which side dials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The source dials the target and sends.
    Push,
    /// The target dials the source and fetches.
    Pull,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pull => "pull",
        }
    }
}

/// One of the two servers of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Active,
    Passive,
}

/// What the scripts of one job need to know.
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub job: String,
    pub mode: Mode,
    /// The passive side as the active side dials it.
    pub peer_host: String,
    pub peer_port: u16,
    pub peer_user: String,
    pub workers: usize,
    pub keep: bool,
    pub verbose: bool,
    /// Seconds a job that ended waits for ferge to collect it before it
    /// tidies itself.
    pub linger: u64,
}

impl Config {
    /// The job folder relative to the login directory.
    pub(crate) fn dir(&self) -> String {
        format!("{JOBS_DIR}/{}", self.job)
    }

    /// The side that receives the files.
    pub(crate) fn target_role(&self) -> Role {
        match self.mode {
            Mode::Push => Role::Passive,
            Mode::Pull => Role::Active,
        }
    }
}

/// One file of a job.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub index: usize,
    pub offset: u64,
    pub size: u64,
    /// `touch -t` time in UTC (`CCYYMMDDhhmm.SS`), if known.
    pub mtime: Option<String>,
    /// Whether the file replaces an existing one.
    pub replace: bool,
    /// The file on the active side (push: source, pull: target).
    pub active_path: String,
    /// The file on the passive side (push: target, pull: source).
    pub passive_path: String,
}

impl Entry {
    fn source_path(&self, mode: Mode) -> &str {
        match mode {
            Mode::Push => &self.active_path,
            Mode::Pull => &self.passive_path,
        }
    }

    fn target_path(&self, mode: Mode) -> &str {
        match mode {
            Mode::Push => &self.passive_path,
            Mode::Pull => &self.active_path,
        }
    }
}

/// Whether `text` can be used unquoted in scripts and `ssh` arguments.
pub(crate) fn is_plain(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-@[]".contains(&byte))
}

const HEADER: &str = "# ferge direct transfer, job @JOB@.\n# Generated; see crates/ferge-core/src/transfer/direct/script.rs.\n";

const LIB: &str = r#"
# pick_clock: defines now, a time in seconds for measuring durations, and
# names its source in CLOCK: GNU date with nanoseconds, else the uptime in
# hundredths (Linux, also with busybox; read without starting a process),
# else whole seconds.
pick_clock() {
    case $(date +%N 2>/dev/null) in
    [0-9]*)
        CLOCK=epoch-ns
        now() { date +%s.%N; }
        ;;
    *)
        if [ -r /proc/uptime ]; then
            CLOCK=uptime
            now() { read -r _now _rest < /proc/uptime; printf '%s\n' "$_now"; }
        else
            CLOCK=epoch-s
            now() { date +%s; }
        fi
        ;;
    esac
}

# size FILE: its size in bytes, 0 if it does not exist.
size() {
    if [ ! -e "$1" ]; then echo 0; return; fi
    set -f
    set -- $(LC_ALL=C ls -ldn "$1")
    set +f
    printf '%s\n' "$5"
}

# digits WORD: whether WORD is a decimal number.
digits() {
    case $1 in ''|*[!0-9]*) return 1 ;; esac
    return 0
}

# send PATH OFFSET: writes PATH from OFFSET on to standard output.
send() {
    if [ "$2" -eq 0 ]; then cat "$1"; else tail -c +$(($2 + 1)) "$1"; fi
}

# place FILE PATH REPLACE: moves FILE to PATH, replacing an existing file
# only if REPLACE is 1. Prints why it failed.
place() {
    if [ "$3" = 1 ]; then
        mv -f "$1" "$2" || { echo "rename-failed"; return 1; }
    elif ln "$1" "$2" 2>/dev/null; then
        rm -f "$1"
    elif [ -e "$2" ] || [ -L "$2" ]; then
        echo "exists"
        return 1
    else
        mv "$1" "$2" || { echo "rename-failed"; return 1; }
    fi
}

# recv PATH OFFSET SIZE MTIME REPLACE: writes standard input to
# PATH.ferge-part from OFFSET on; once it holds SIZE bytes, sets MTIME and
# moves it to PATH. Prints "ok BYTES" or why it failed.
recv() {
    _part="$1.ferge-part"
    if [ "$2" -eq 0 ]; then
        cat > "$_part" || { echo "write-failed"; return 1; }
    else
        _have=$(size "$_part")
        if [ "$_have" -lt "$2" ]; then
            cat > /dev/null
            echo "resume-gap $_have"
            return 1
        fi
        if [ "$_have" -gt "$2" ]; then
            dd if=/dev/null of="$_part" bs=1 seek="$2" 2>/dev/null \
                || { cat > /dev/null; echo "truncate-failed"; return 1; }
        fi
        cat >> "$_part" || { echo "write-failed"; return 1; }
    fi
    _got=$(size "$_part")
    if [ "$_got" -ne "$3" ]; then
        echo "size $_got of $3"
        return 1
    fi
    if [ "$4" != - ]; then
        TZ=UTC0 touch -t "$4" "$_part" 2>/dev/null
    fi
    place "$_part" "$1" "$5" || return 1
    echo "ok $_got"
}

# send_batch BATCH: writes the batch's files to standard output as one tar
# stream (its `t` line names the folder and the files below it).
send_batch() {
    _code=1
    i() { :; }
    g() { :; }
    t() {
        _root=$1
        shift
        # GNU tar reads backslashes in names as escapes unless told not to.
        case $(tar --version 2>/dev/null) in
        *GNU*) set -- --no-unquote "$@" ;;
        esac
        tar -chf - -C "$_root" "$@"
        _code=$?
    }
    . "./batch/$1.sh"
    return $_code
}

# recv_batch BATCH: unpacks a stream from send_batch into the staging folder,
# checks each file's size and moves the files of each folder into place at
# once (`g` lines). Prints "d FILE BYTES" or "e FILE REASON" per file ("e -
# REASON" for the whole batch).
recv_batch() {
    STAGE=
    [ -f ./stage ] && . ./stage
    case ${STAGE##*/} in
    .ferge-j*) ;;
    *) cat > /dev/null; echo "e - no-stage"; return 1 ;;
    esac
    _batch=$1
    _in="$STAGE/$1"
    [ -d "$STAGE" ] || mkdir "$STAGE" 2>/dev/null
    rm -rf "$_in"
    if ! mkdir "$_in"; then cat > /dev/null; echo "e - stage-failed"; return 1; fi
    tar -xf - -C "$_in"
    _tar=$?
    # The stream of send_batch holds files only.
    if [ -n "$(find "$_in" -type l 2>/dev/null | head -n 1)" ]; then
        rm -rf "$_in"
        echo "e - unsafe-stream"
        return 1
    fi
    # Files tar stored as links to each other get copies of their own.
    find "$_in" -type f -links +1 > "links.$1" 2>/dev/null
    while IFS= read -r _f; do
        cp -p "$_f" "$_f.ferge-copy" && mv -f "$_f.ferge-copy" "$_f"
    done < "links.$1"
    rm -f "links.$1"
    i() { :; }
    t() { :; }
    g() { place_group "$@"; }
    . "./batch/$1.sh"
    rm -rf "$_in"
    return $_tar
}

# place_group FOLDER TARGET [FILE SIZE REPLACE NAME]...: checks the files
# that arrived in FOLDER (below the batch's staging folder) against their
# sizes, all with one `wc`, and moves them to the folder TARGET: new ones
# with one `ln` that never replaces anything, the others with one `mv -f`.
place_group() {
    _dir="$_in/$1"
    _to=$2
    shift 2
    _arrived=
    while [ $# -ge 4 ]; do
        if [ -f "$_dir/$4" ] && [ ! -L "$_dir/$4" ]; then
            _arrived="$_arrived$1 $2 $3 $4
"
        else
            echo "e $1 missing"
        fi
        shift 4
    done
    set --
    while read -r _i _s _r _n; do
        [ -n "$_i" ] && set -- "$@" "$_dir/$_n"
    done <<EOF
$_arrived
EOF
    [ $# -gt 0 ] || return 0
    # One line per file, in order (and a total after several).
    wc -c "$@" > "sizes.$_batch" 2>/dev/null
    exec 3< "sizes.$_batch"
    _kept=
    while read -r _i _s _r _n; do
        [ -n "$_i" ] || continue
        read -r _got _rest <&3
        if [ "$_got" = "$_s" ]; then
            _kept="$_kept$_i $_s $_r $_n
"
        else
            echo "e $_i size ${_got:-0} of $_s"
        fi
    done <<EOF
$_arrived
EOF
    exec 3<&-
    rm -f "sizes.$_batch"
    for _replace in 0 1; do
        set --
        while read -r _i _s _r _n; do
            [ "$_r" = "$_replace" ] && set -- "$@" "$_dir/$_n"
        done <<EOF
$_kept
EOF
        [ $# -gt 0 ] || continue
        if [ "$_replace" = 1 ]; then
            mv -f "$@" "$_to/" 2>/dev/null
        else
            ln "$@" "$_to/" 2>/dev/null
        fi
    done
    while read -r _i _s _r _n; do
        [ -n "$_i" ] || continue
        if [ "$_r" = 1 ]; then
            if [ -e "$_dir/$_n" ]; then
                _why=$(place "$_dir/$_n" "$_to/$_n" 1) || { echo "e $_i $_why"; continue; }
            fi
        elif ! [ "$_dir/$_n" -ef "$_to/$_n" ]; then
            if [ -e "$_to/$_n" ] || [ -L "$_to/$_n" ]; then
                echo "e $_i exists"
                continue
            fi
            # ln fails across file systems; mv copies then.
            _why=$(place "$_dir/$_n" "$_to/$_n" 0) || { echo "e $_i $_why"; continue; }
        fi
        echo "d $_i $_s"
    done <<EOF
$_kept
EOF
}

# unstage: removes the job's staging folder (run in the job folder), once
# batches still being received have moved what arrived into place.
unstage() {
    _tries=0
    for _busy in busy.*; do
        [ -f "$_busy" ] || continue
        while kill -0 "$(cat "$_busy" 2>/dev/null)" 2>/dev/null && [ "$_tries" -lt 100 ]; do
            sleep 0.1 2>/dev/null || sleep 1
            _tries=$((_tries + 1))
        done
        rm -f "$_busy"
    done
    STAGE=
    [ -f ./stage ] && . ./stage
    case ${STAGE##*/} in .ferge-j*) rm -rf "$STAGE" ;; esac
    return 0
}
"#;

/// What ferge changes outside its job folders, and how it puts it back. All
/// of it runs from the login directory.
const FOOTPRINT: &str = r#"
# is_job WORD: whether WORD looks like a job id.
is_job() {
    case $1 in j|j*[!0-9a-f]*) return 1 ;; j*) return 0 ;; esac
    return 1
}

# lock, unlock: jobs take turns editing authorized_keys; a lock older than
# 30 seconds is taken over.
lock() {
    _tries=0
    until mkdir .ssh/.ferge-lock 2>/dev/null || [ "$_tries" -ge 30 ]; do
        sleep 1
        _tries=$((_tries + 1))
    done
}
unlock() { rmdir .ssh/.ferge-lock 2>/dev/null; }

# state PATH: what PATH holds (its entries, or its contents), as a checksum.
state() {
    if [ -d "$1" ]; then LC_ALL=C ls -a "$1" 2>/dev/null | cksum; else cksum < "$1"; fi
}

# note NAME PATH: prints whether PATH exists, its modification time, its
# state and (for a folder) its entries; with REFS set, also keeps its times
# and state there for restore.
note() {
    if [ ! -e "$2" ]; then
        echo "note $1 -"
        return 0
    fi
    _state=$(state "$2")
    echo "note $1 $(date -r "$2" +%s.%N 2>/dev/null) $_state"
    if [ -d "$2" ]; then
        LC_ALL=C ls -a "$2" 2>/dev/null | sed "s/^/entry $1 /"
    fi
    if [ -n "$REFS" ]; then
        touch -r "$2" "$REFS/$1" 2>/dev/null && printf '%s\n' "$_state" > "$REFS/$1.state"
    fi
    return 0
}

# restore NAME PATH: gives PATH its noted times back if it holds again what
# it held then.
restore() {
    [ -n "$REFS" ] && [ -f "$REFS/$1" ] && [ -e "$2" ] || return 0
    [ "$(state "$2")" = "$(cat "$REFS/$1.state" 2>/dev/null)" ] || return 0
    touch -r "$REFS/$1" "$2" 2>/dev/null
    return 0
}

# unkey JOB: removes the job's line from authorized_keys and what ferge added
# for it (a final newline, the file, the .ssh folder), and restores the
# times if nothing else changed meanwhile.
unkey() {
    is_job "$1" || return 1
    _k=.ssh/authorized_keys
    [ -f "$_k" ] && grep "ferge-job-$1" "$_k" > /dev/null 2>&1 || return 0
    REFS=.cache/ferge/jobs/$1/before
    [ -d "$REFS" ] || REFS=
    lock
    _line=$(grep "ferge-job-$1" "$_k" | tail -n 1)
    _last=$(tail -n 1 "$_k")
    _made=,
    case $_line in *" made="*) _made=",${_line##* made=}," ;; esac
    _tmp=$_k.ferge-$1
    sed "/ferge-job-$1/d" "$_k" > "$_tmp"
    case $_made in
    *,newline,*)
        if [ "$_line" = "$_last" ]; then
            printf '%s' "$(cat "$_tmp")" > "$_tmp.1" && mv -f "$_tmp.1" "$_tmp"
        fi
        ;;
    esac
    cat "$_tmp" > "$_k"
    rm -f "$_tmp" "$_tmp.1"
    case $_made in *,file,*) [ -s "$_k" ] || rm -f "$_k" ;; esac
    restore keys "$_k"
    unlock
    case $_made in
    *,dir,*) rmdir .ssh 2>/dev/null ;;
    *) restore ssh .ssh ;;
    esac
    REFS=
    echo "unkeyed $1"
}

# tidy JOB: removes the job folder, then the folders ferge made for jobs once
# none is left, and restores the times of the folders that held them.
tidy() {
    is_job "$1" || return 1
    _j=.cache/ferge/jobs/$1
    REFS=
    if [ -d "$_j/before" ]; then
        REFS=$(mktemp -d 2>/dev/null) && cp -p "$_j/before/"* "$REFS/" 2>/dev/null
    fi
    rm -rf "$_j"
    if rmdir .cache/ferge/jobs 2>/dev/null; then
        _cache=
        [ -f .cache/ferge/made-cache ] && _cache=1
        rm -f .cache/ferge/made-cache
        if rmdir .cache/ferge 2>/dev/null; then
            [ -n "$_cache" ] && rmdir .cache 2>/dev/null
        elif [ -n "$_cache" ]; then
            : > .cache/ferge/made-cache
        fi
    fi
    if [ -n "$REFS" ]; then
        restore jobs .cache/ferge/jobs
        restore ferge .cache/ferge
        restore cache .cache
        restore home .
        rm -rf "$REFS"
    fi
    REFS=
    echo "tidied $1"
}

# young PATH: whether PATH changed in the last half hour (or that is unknown).
young() {
    _found=$(find "$1" -maxdepth 0 -mmin -30 2>/dev/null) || return 0
    [ -n "$_found" ]
}

# keyed: the jobs with a line in authorized_keys.
keyed() {
    [ -f .ssh/authorized_keys ] || return 0
    sed -n 's/.*ferge-job-\(j[0-9a-f]*\).*/\1/p' .ssh/authorized_keys
}

# running FOLDER: whether the driver of that job still runs.
running() {
    [ -s "$1/pid" ] && kill -0 "$(cat "$1/pid")" 2>/dev/null
}

# sweep: removes what earlier jobs left behind, except the job JOB: keys of
# jobs whose folder is gone or that expired, folders of jobs that no longer
# run, a stale lock. Prints what it removed, and the result a job kept when
# it ended without ferge.
sweep() {
    _today=$(date +%Y%m%d)
    for _id in $(keyed); do
        [ "$_id" = "$JOB" ] && continue
        _until=$(sed -n "s/.*expiry-time=\"\([0-9]*\)\".*ferge-job-$_id.*/\1/p" .ssh/authorized_keys | head -n 1)
        _until=${_until%????}
        if [ ! -d ".cache/ferge/jobs/$_id" ] || { digits "$_until" && [ "$_until" -lt "$_today" ]; }; then
            unkey "$_id" > /dev/null && echo "swept key $_id"
        fi
    done
    for _d in .cache/ferge/jobs/j*; do
        [ -d "$_d" ] || continue
        _id=${_d##*/}
        [ "$_id" = "$JOB" ] && continue
        if [ -f "$_d/result" ]; then
            echo "result $_id"
            sed 's/^/| /' "$_d/result"
        elif [ -f "$_d/driver.sh" ]; then
            running "$_d" && continue
            young "$_d" && continue
            echo "result $_id"
            tail -n 20 "$_d/status" 2>/dev/null | sed 's/^/| /'
        else
            grep "ferge-job-$_id" .ssh/authorized_keys > /dev/null 2>&1 && continue
            young "$_d" && continue
        fi
        (cd "$_d" && unstage)
        tidy "$_id" > /dev/null
        echo "swept job $_id"
    done
    if [ -d .ssh/.ferge-lock ] && ! young .ssh/.ferge-lock; then
        rmdir .ssh/.ferge-lock 2>/dev/null && echo "swept lock"
    fi
    return 0
}

# notes: the state of everything ferge adds to.
notes() {
    note home .
    note cache .cache
    note ferge .cache/ferge
    note jobs .cache/ferge/jobs
    note ssh .ssh
    note keys .ssh/authorized_keys
}
"#;

const PEER: &str = r#"
# peer_ssh ARGS: ssh to the other side with nothing but the job's key and
# host key, ignoring the account's own SSH configuration.
peer_ssh() {
    ssh -F /dev/null -i ./key -o IdentitiesOnly=yes -o IdentityAgent=none \
        -o BatchMode=yes -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \
        -o UserKnownHostsFile=./known_hosts -o GlobalKnownHostsFile=/dev/null \
        -o HostKeyAlias=ferge-peer -o StrictHostKeyChecking=yes -o CheckHostIP=no \
        -o ControlPath=./ctl -o ServerAliveInterval=15 -o ServerAliveCountMax=4 \
        -o ConnectTimeout=15 -o LogLevel=$PEER_LOG_LEVEL \
        -p "$PEER_PORT" -l "$PEER_USER" "$@"
}

# One connection carries all requests; it ends a minute after the last one.
peer_open() { peer_ssh -o ControlMaster=yes -o ControlPersist=60 -f -N "$PEER_HOST"; }
peer_close() { peer_ssh -O exit "$PEER_HOST" 2>/dev/null; }
peer() { peer_ssh -o ControlMaster=no "$PEER_HOST" "$*"; }
peer_once() { peer_ssh -o ControlMaster=no -o ControlPath=none "$PEER_HOST" "$*"; }
"#;

const DRIVER: &str = r#"
cd "$(dirname "$0")" || exit 90
. ./lib.sh
. ./peer.sh
pick_clock

case $1 in
probe)
    for _tool in ssh cat tail dd ls ln mv cp touch date tar sed grep cksum find mktemp setsid nohup uname; do
        printf 'tool %s %s\n' "$_tool" "$(command -v "$_tool" 2>/dev/null)"
    done
    printf 'system %s\n' "$(uname -srm 2>/dev/null)"
    printf 'ssh %s\n' "$(ssh -V 2>&1)"
    printf 'clock-source %s\n' "$CLOCK"
    printf 'clock %s\n' "$(now)"
    _reply=$(peer_once probe 2>./err.probe)
    _code=$?
    printf 'clock %s\n' "$(now)"
    printf 'peer-code %s\n' "$_code"
    printf 'peer-reply %s\n' "$_reply"
    if [ "$_code" -eq 0 ]; then
        case $_reply in "ferge-passive "*) exit 0 ;; esac
    fi
    exit 1
    ;;
run) ;;
*) echo "usage: driver.sh probe|run" >&2; exit 64 ;;
esac

# f FILE OFFSET SIZE MTIME REPLACE PATH: copies one file.
f() {
    echo "s $1 $(now)" >> status
    if [ "$MODE" = push ]; then
        _r=$(send "$6" "$2" | peer put "$1" "$2")
    else
        _r=$(peer get "$1" "$2" | recv "$6" "$2" "$3" "$4" "$5")
    fi
    _code=$?
    case $_r in
    "ok "*) echo "d $1 ${_r#ok } $(now)" >> status ;;
    *) echo "e $1 $_code $(now) $(printf '%s' "$_r" | tr '\n' ' ')" >> status ;;
    esac
}

# b BATCH: copies a batch of small files in one stream.
b() {
    echo "S $1 $(now)" >> status
    if [ "$MODE" = push ]; then
        send_batch "$1" | peer putb "$1" > "res.$1"
    else
        peer getb "$1" | recv_batch "$1" > "res.$1"
    fi
    _code=$?
    _t=$(now)
    _why=
    {
        while read -r _k _i _rest; do
            if [ "$_i" = - ]; then _why=$_rest; continue; fi
            digits "$_i" || continue
            case $_k in
            d) echo "d $_i $_rest $_t" ;;
            e) echo "e $_i 1 $_t $_rest" ;;
            *) continue ;;
            esac
            eval "_seen_$_i=1"
        done < "res.$1"
        i() {
            for _i do
                eval "_v=\${_seen_$_i-}; unset _seen_$_i"
                [ -n "$_v" ] || echo "e $_i $_code $_t ${_why:-batch-failed}"
            done
        }
        t() { :; }
        g() { :; }
        . "./batch/$1.sh"
    } >> status
    echo "X $1 $_code $_t" >> status
    rm -f "res.$1"
}

# compact: what is left of a job that ended without ferge: its status (for
# ferge to follow it later) and a short result with the ends of its logs.
compact() {
    {
        echo "job @JOB@ ended without ferge"
        tail -n 1 status
        for _e in log err.*; do
            [ -s "$_e" ] || continue
            echo "--- $_e"
            tail -c 8192 "$_e"
        done
    } 2>/dev/null | tail -c 65536 > ./result.part
    for _f in ./*; do
        case $_f in ./result.part|./before|./status) ;; *) rm -rf "$_f" ;; esac
    done
    mv ./result.part ./result
}

# linger: gives ferge a while to collect the job (it removes the folder),
# then tidies it.
linger() {
    [ "$KEEP" = 1 ] && return 0
    (
        _waited=0
        while [ -f ./driver.sh ] && [ "$_waited" -lt @LINGER@ ]; do
            sleep 1
            _waited=$((_waited + 1))
        done
        [ -f ./driver.sh ] && compact
    ) </dev/null >/dev/null 2>&1 &
}

echo $$ > pid
trap 'trap "" TERM INT; echo "E 143 $(now)" >> status; kill 0 2>/dev/null; unstage; peer cleanup >/dev/null 2>&1; peer_close; rm -f key; exit 143' TERM INT
echo "B $(now) $$" >> status
if ! peer_open 2>./err.master; then
    echo "M fail $(now)" >> status
    rm -f key
    unstage
    echo "E 70 $(now)" >> status
    linger
    exit 70
fi
echo "M ok $(now)" >> status
_worker=0
while [ "$_worker" -lt "$WORKERS" ]; do
    if [ -f "list.$_worker" ]; then
        ( . "./list.$_worker" ) 2>>"err.$_worker" &
    fi
    _worker=$((_worker + 1))
done
wait
unstage
peer cleanup >/dev/null 2>>./err.master
peer_close
rm -f key
trap - TERM INT
echo "E 0 $(now)" >> status
linger
"#;

const START: &str = r#"
cd "$(dirname "$0")" || exit 90
rm -f pid
if command -v setsid >/dev/null 2>&1; then
    setsid nohup sh ./driver.sh run </dev/null >>log 2>&1 &
else
    nohup sh ./driver.sh run </dev/null >>log 2>&1 &
fi
# Report the driver's own pid once it runs.
_tries=0
while [ ! -s pid ] && [ "$_tries" -lt 250 ]; do
    sleep 0.02 2>/dev/null || sleep 1
    _tries=$((_tries + 1))
done
echo "started $(cat pid 2>/dev/null)"
"#;

const STOP: &str = r#"
cd "$(dirname "$0")" || exit 90
. ./lib.sh
. ./peer.sh
if [ -s pid ]; then
    _pid=$(cat pid)
    kill -TERM -"$_pid" 2>/dev/null || kill -TERM "$_pid" 2>/dev/null
    # The driver removes the key on the other side itself; give it a moment.
    _tries=0
    while kill -0 "$_pid" 2>/dev/null && [ "$_tries" -lt 50 ]; do
        sleep 0.1 2>/dev/null || sleep 1
        _tries=$((_tries + 1))
    done
fi
peer_close
rm -f key
unstage
echo "stopped"
"#;

const ALIVE: &str = r#"
cd "$(dirname "$0")" || exit 90
if [ -s pid ] && kill -0 "$(cat pid)" 2>/dev/null; then echo alive; else echo dead; fi
"#;

const PASSIVE: &str = r#"
cd "$(dirname "$0")" || exit 90
. ./lib.sh

# entry FILE: sets p (path), s (size), m (mtime) and r (replace) of FILE.
entry() {
    p=
    _table="tab/$(($1 / @TABLE_SIZE@)).sh"
    [ -f "$_table" ] || return 1
    . "./$_table"
    [ -n "$p" ]
}

set -f
set -- $SSH_ORIGINAL_COMMAND
set +f
case $1 in
probe)
    printf 'ferge-passive @JOB@ tar=%s %s\n' "$(command -v tar 2>/dev/null)" "$(uname -srm 2>/dev/null)"
    exit 0
    ;;
cleanup) sh ./uninstall.sh @KEEP@; exit 0 ;;
putb|getb)
    if ! digits "$2" || [ ! -f "batch/$2.sh" ]; then echo "ferge: no batch $2 in this job" >&2; exit 65; fi
    if [ "$1" = getb ]; then send_batch "$2"; exit $?; fi
    # Marks the batch as being received, for unstage.
    echo $$ > "busy.$2"
    recv_batch "$2"
    _code=$?
    rm -f "busy.$2"
    exit $_code
    ;;
put|get) ;;
*) echo "ferge: unknown request" >&2; exit 64 ;;
esac
if ! digits "$2" || ! digits "$3"; then echo "ferge: bad request" >&2; exit 64; fi
if ! entry "$2"; then echo "ferge: no file $2 in this job" >&2; exit 65; fi
case $1 in
put) recv "$p" "$3" "$s" "$m" "$r" ;;
get) send "$p" "$3" ;;
esac
"#;

const INSTALL: &str = r#"
cd "$(dirname "$0")" || exit 90
. ./lib.sh
cd "$HOME" || exit 90
umask 077
J=@DIR@
REFS=$J/before
note ssh .ssh
_made=
if [ ! -d .ssh ]; then
    mkdir .ssh || exit 1
    _made=dir
fi
lock
_k=.ssh/authorized_keys
note keys "$_k"
if [ ! -f "$_k" ]; then
    if ! : > "$_k"; then unlock; exit 1; fi
    _made="$_made,file"
elif [ -s "$_k" ] && [ -n "$(tail -c 1 "$_k")" ]; then
    # The line must start on its own.
    echo >> "$_k"
    _made="$_made,newline"
fi
_made=${_made#,}
_line=$(cat "$J/authorized_key")
[ -n "$_made" ] && _line="$_line made=$_made"
if printf '%s\n' "$_line" >> "$_k"; then echo installed; fi
unlock
"#;

const UNINSTALL: &str = r#"
cd "$(dirname "$0")" || exit 90
. ./lib.sh
unstage
cd "$HOME" || exit 90
unkey @JOB@
if [ "$1" != keep ]; then
    tidy @JOB@ > /dev/null
fi
echo removed
"#;

/// Sweeps, notes the state and makes the job folder (`sh -s`).
const SETUP: &str = r#"
umask 077
J=.cache/ferge/jobs/$JOB
sweep
REFS=$(mktemp -d 2>/dev/null) || REFS=
notes
_made=
[ -d .cache ] || _made=1
if ! mkdir -p "$J/before"; then
    [ -n "$REFS" ] && rm -rf "$REFS"
    exit 1
fi
[ -n "$_made" ] && : > .cache/ferge/made-cache
if [ -n "$REFS" ]; then
    cp -p "$REFS"/* "$J/before/" 2>/dev/null
    rm -rf "$REFS"
fi
REFS=
echo "zone $(date +%z)"
echo ready
"#;

fn script(config: &Config, body: &str) -> String {
    let mut text = String::from("#!/bin/sh\n");
    text.push_str(HEADER);
    text.push_str(body);
    text.replace("@DIR@", &config.dir())
        .replace("@JOB@", &config.job)
        .replace("@TABLE_SIZE@", &TABLE_SIZE.to_string())
        .replace("@KEEP@", if config.keep { "keep" } else { "" })
        .replace("@LINGER@", &config.linger.to_string())
}

/// A script for `sh -s`, run from the login directory with the footprint
/// functions; `job` is the job it is about (if any).
fn stdin_script(job: Option<&str>, body: &str) -> String {
    let mut text = String::from("cd \"$HOME\" || exit 90\n");
    let _ = writeln!(text, "JOB={}", job.unwrap_or(""));
    text.push_str(LIB);
    text.push_str(FOOTPRINT);
    text.push_str(body);
    text
}

pub(crate) fn lib_sh(config: &Config) -> String {
    script(config, &format!("{LIB}{FOOTPRINT}"))
}

pub(crate) fn peer_sh(config: &Config) -> String {
    let mut body = String::new();
    let _ = writeln!(body, "PEER_HOST={}", quote(&config.peer_host));
    let _ = writeln!(body, "PEER_PORT={}", config.peer_port);
    let _ = writeln!(body, "PEER_USER={}", quote(&config.peer_user));
    let _ = writeln!(
        body,
        "PEER_LOG_LEVEL={}",
        if config.verbose { "DEBUG1" } else { "ERROR" }
    );
    let _ = writeln!(body, "MODE={}", config.mode.as_str());
    let _ = writeln!(body, "WORKERS={}", config.workers.max(1));
    let _ = writeln!(body, "KEEP={}", u8::from(config.keep));
    body.push_str(PEER);
    script(config, &body)
}

pub(crate) fn driver_sh(config: &Config) -> String {
    script(config, DRIVER)
}

pub(crate) fn start_sh(config: &Config) -> String {
    script(config, START)
}

pub(crate) fn stop_sh(config: &Config) -> String {
    script(config, STOP)
}

pub(crate) fn alive_sh(config: &Config) -> String {
    script(config, ALIVE)
}

pub(crate) fn passive_sh(config: &Config) -> String {
    script(config, PASSIVE)
}

pub(crate) fn install_sh(config: &Config) -> String {
    script(config, INSTALL)
}

pub(crate) fn uninstall_sh(config: &Config) -> String {
    script(config, UNINSTALL)
}

/// Removes leftovers of earlier jobs, notes the state of what ferge adds to
/// and makes the job folder. Prints `note`/`entry` lines, what it swept,
/// `zone <+hhmm>` and finally `ready`.
pub(crate) fn setup_script(config: &Config) -> String {
    stdin_script(Some(&config.job), SETUP)
}

/// What ferge runs on one side when a job is over.
#[derive(Clone, Debug, Default)]
pub(crate) struct Finish<'a> {
    /// The side's staging folder, to remove.
    pub stage: Option<&'a str>,
    /// Remove the job's key line (the passive side).
    pub unkey: bool,
    /// Keep the job folder (only remove the private key).
    pub keep: bool,
}

/// Removes what the job added on one side, then notes the state again and
/// says what is left (`left …` lines).
pub(crate) fn finish_script(config: &Config, finish: &Finish<'_>) -> String {
    let mut body = String::from("J=.cache/ferge/jobs/$JOB\nSTAGE=\n");
    if let Some(stage) = finish.stage {
        body.push_str("[ -d \"$J\" ] && (cd \"$J\" && unstage)\n");
        let _ = writeln!(body, "STAGE={}", quote(stage));
        body.push_str("case ${STAGE##*/} in .ferge-j*) rm -rf \"$STAGE\" ;; esac\n");
    }
    if finish.unkey {
        body.push_str("unkey \"$JOB\"\n");
    }
    if finish.keep {
        body.push_str("rm -f \"$J/key\"\necho kept\n");
    } else {
        body.push_str(
            "tidy \"$JOB\"\n\
             REFS=\n\
             notes\n\
             [ -e \"$J\" ] && echo \"left folder\"\n\
             [ -n \"$STAGE\" ] && [ -e \"$STAGE\" ] && echo \"left stage\"\n\
             grep \"ferge-job-$JOB\" .ssh/authorized_keys > /dev/null 2>&1 && echo \"left key\"\n\
             [ -d .ssh/.ferge-lock ] && echo \"left lock\"\n",
        );
    }
    body.push_str("echo finished\n");
    stdin_script(Some(&config.job), &body)
}

/// Gives the job's key a new expiry time (`YYYYMMDDHHMM` in the server's
/// time). Prints `renewed`.
pub(crate) fn renew_script(job: &str, expiry: &str) -> String {
    let body = format!(
        "_k=.ssh/authorized_keys\n\
         [ -f \"$_k\" ] || exit 1\n\
         lock\n\
         sed \"/ferge-job-$JOB/s/expiry-time=\\\"[0-9]*\\\"/expiry-time=\\\"{expiry}\\\"/\" \"$_k\" > \"$_k.ferge-$JOB\" \
         && cat \"$_k.ferge-$JOB\" > \"$_k\"\n\
         rm -f \"$_k.ferge-$JOB\"\n\
         unlock\n\
         grep \"ferge-job-$JOB\" \"$_k\" | grep 'expiry-time=\"{expiry}\"' > /dev/null && echo renewed\n"
    );
    stdin_script(Some(job), &body)
}

const INSPECT: &str = r#"
for _d in .cache/ferge/jobs/j*; do
    [ -d "$_d" ] || continue
    _id=${_d##*/}
    if [ -f "$_d/result" ]; then
        _kind=ended
    elif [ -f "$_d/driver.sh" ]; then
        if running "$_d"; then _kind=running; else _kind=stopped; fi
    elif [ -f "$_d/passive.sh" ]; then
        _kind=answering
    else
        _kind=unknown
    fi
    echo "job $_id $_kind $(date -r "$_d" +%s 2>/dev/null)"
    STAGE=
    [ -f "$_d/stage" ] && . "$_d/stage"
    [ -n "$STAGE" ] && [ -e "$STAGE" ] && printf 'stage %s %s\n' "$_id" "$STAGE"
done
if [ -f .ssh/authorized_keys ]; then
    sed -n 's/.*expiry-time="\([0-9]*\)".*ferge-job-\(j[0-9a-f]*\).*/key \2 \1/p' .ssh/authorized_keys
fi
[ -d .ssh/.ferge-lock ] && echo "lock"
[ -d .cache/ferge ] && echo "folder .cache/ferge"
echo inspected
"#;

const PURGE: &str = r#"
for _d in .cache/ferge/jobs/j*; do
    [ -d "$_d" ] || continue
    _id=${_d##*/}
    if [ -f "$_d/stop.sh" ] && running "$_d"; then
        sh "$_d/stop.sh" > /dev/null 2>&1
        echo "stopped $_id"
    fi
    (cd "$_d" && unstage)
done
for _id in $(keyed); do
    unkey "$_id"
done
for _d in .cache/ferge/jobs/j*; do
    [ -d "$_d" ] && tidy "${_d##*/}"
done
rmdir .ssh/.ferge-lock 2>/dev/null
rmdir .cache/ferge/jobs .cache/ferge 2>/dev/null
echo purged
"#;

/// Lists ferge's jobs, keys and staging folders on a server; changes
/// nothing.
pub(crate) fn inspect_script() -> String {
    stdin_script(None, INSPECT)
}

/// Stops every ferge job on a server and removes all of them: keys, folders,
/// staging folders.
pub(crate) fn purge_script() -> String {
    stdin_script(None, PURGE)
}

/// The `authorized_keys` line for the job's key: usable only for the job's
/// forced command, without forwarding or a terminal, until `expiry`
/// (`YYYYMMDDHHMM` in the server's time).
pub(crate) fn authorized_key(config: &Config, public_key: &str, expiry: &str) -> String {
    format!(
        "restrict,expiry-time=\"{expiry}\",command=\"sh {}/passive.sh\" {} ferge-job-{}\n",
        config.dir(),
        key_material(public_key),
        config.job
    )
}

/// The `known_hosts` line that makes the passive side's key the only one
/// accepted, under the alias the driver uses.
pub(crate) fn known_hosts(host_key: &str) -> String {
    format!("ferge-peer {}\n", key_material(host_key))
}

/// `type base64` of an OpenSSH public key line, without its comment.
fn key_material(key: &str) -> String {
    key.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

/// The files of a job, split into those copied alone and batches.
#[derive(Debug, Default)]
pub(crate) struct Work {
    pub singles: Vec<Entry>,
    pub batches: Vec<Vec<Entry>>,
}

/// Most bytes of names a batch passes to `tar` and `ln`.
const BATCH_NAME_BYTES: usize = 96 * 1024;

/// `/a/b/c` → (`/a/b`, `c`).
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    }
}

/// Whether a file can go in a batch: it starts from the beginning, is
/// small, keeps its name, and its source path survives the lines of `wc`
/// and `find` (no control characters; the name neither starts nor ends with
/// a blank).
fn batchable(entry: &Entry, mode: Mode) -> bool {
    let (source, target) = (entry.source_path(mode), entry.target_path(mode));
    let name = split_path(source).1;
    entry.offset == 0
        && entry.size <= BATCH_FILE_LIMIT
        && source.starts_with('/')
        && target.starts_with('/')
        && !name.is_empty()
        && name == split_path(target).1
        && !source.chars().any(char::is_control)
        && !name.starts_with(' ')
        && !name.ends_with(' ')
}

/// Groups small files into batches (if `batching`), folder by folder so a
/// batch mostly holds one folder; every worker gets a few batches.
pub(crate) fn split_work(entries: Vec<Entry>, workers: usize, batching: bool, mode: Mode) -> Work {
    let (mut small, singles): (Vec<Entry>, Vec<Entry>) = entries
        .into_iter()
        .partition(|entry| batching && batchable(entry, mode));
    // Folder by folder.
    small.sort_by(|a, b| split_path(a.source_path(mode)).cmp(&split_path(b.source_path(mode))));
    let per_batch = small
        .len()
        .div_ceil(workers.max(1) * 4)
        .clamp(BATCH_FILES.0, BATCH_FILES.1);
    let mut batches: Vec<Vec<Entry>> = Vec::new();
    let (mut bytes, mut names) = (0, 0);
    for entry in small {
        let name_bytes = entry.source_path(mode).len() + entry.target_path(mode).len();
        let full = batches.last().is_none_or(|batch| {
            batch.len() >= per_batch
                || bytes + entry.size > BATCH_BYTES
                || names + name_bytes > BATCH_NAME_BYTES
        });
        if full {
            batches.push(Vec::new());
            (bytes, names) = (0, 0);
        }
        bytes += entry.size;
        names += name_bytes;
        if let Some(batch) = batches.last_mut() {
            batch.push(entry);
        }
    }
    Work { singles, batches }
}

/// The folder all of `folders` are in.
fn common_folder<'a>(folders: impl Iterator<Item = &'a str>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for folder in folders {
        let parts: Vec<&str> = folder.split('/').filter(|part| !part.is_empty()).collect();
        common = Some(match common {
            None => parts,
            Some(common) => common
                .iter()
                .zip(&parts)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    format!("/{}", common.unwrap_or_default().join("/"))
}

/// `folder` below `root` (`""` for the root itself).
fn below<'a>(folder: &'a str, root: &str) -> &'a str {
    if root == "/" {
        folder.trim_start_matches('/')
    } else {
        folder
            .strip_prefix(root)
            .unwrap_or(folder)
            .trim_start_matches('/')
    }
}

/// A batch's file for the sending side (`role` sends) or the receiving one:
/// `i` lists the file numbers; the sender's `t` names the folder to archive
/// from and the files below it; each of the receiver's `g` lines a folder
/// below the staging folder, where its files go, and the files (number,
/// size, replace, name).
pub(crate) fn batch_file(batch: &[Entry], mode: Mode, role: Role) -> String {
    let source_role = match mode {
        Mode::Push => Role::Active,
        Mode::Pull => Role::Passive,
    };
    let mut text = String::from("i");
    for entry in batch {
        let _ = write!(text, " {}", entry.index);
    }
    text.push('\n');
    let root = common_folder(
        batch
            .iter()
            .map(|entry| split_path(entry.source_path(mode)).0),
    );
    if role == source_role {
        let _ = write!(text, "t {}", quote(&root));
        for entry in batch {
            let (folder, name) = split_path(entry.source_path(mode));
            let below = below(folder, &root);
            let member = if below.is_empty() {
                format!("./{name}")
            } else {
                format!("./{below}/{name}")
            };
            let _ = write!(text, " {}", quote(&member));
        }
        text.push('\n');
        return text;
    }
    let mut group: Option<(&str, &str)> = None;
    for entry in batch {
        let (folder, name) = split_path(entry.source_path(mode));
        let target = split_path(entry.target_path(mode)).0;
        if group != Some((folder, target)) {
            if group.is_some() {
                text.push('\n');
            }
            let below = below(folder, &root);
            let _ = write!(
                text,
                "g {} {}",
                quote(if below.is_empty() { "." } else { below }),
                quote(target)
            );
            group = Some((folder, target));
        }
        let _ = write!(
            text,
            " {} {} {} {}",
            entry.index,
            entry.size,
            u8::from(entry.replace),
            quote(name)
        );
    }
    if group.is_some() {
        text.push('\n');
    }
    text
}

/// The driver's lists, one per worker: `f …` lines for files alone, `b <n>`
/// for batches. Large items are spread first, each to the worker with the
/// least work so far; fixed costs account for the round trips per file and
/// per batch.
pub(crate) fn worker_lists(work: &Work, workers: usize) -> Vec<String> {
    const COST_PER_FILE: u64 = 1024 * 1024;
    const COST_PER_BATCHED: u64 = 16 * 1024;
    enum Item<'a> {
        File(&'a Entry),
        Batch(usize),
    }
    let mut items: Vec<(u64, usize, Item<'_>)> = work
        .singles
        .iter()
        .map(|entry| (entry.size + COST_PER_FILE, entry.index, Item::File(entry)))
        .chain(work.batches.iter().enumerate().map(|(number, batch)| {
            let bytes: u64 = batch.iter().map(|entry| entry.size).sum();
            let cost = bytes + COST_PER_FILE + COST_PER_BATCHED * batch.len() as u64;
            let first = batch.first().map_or(0, |entry| entry.index);
            (cost, first, Item::Batch(number))
        }))
        .collect();
    let workers = workers.max(1).min(items.len().max(1));
    items.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut load = vec![0u64; workers];
    let mut lists: Vec<Vec<(usize, Item<'_>)>> = (0..workers).map(|_| Vec::new()).collect();
    for (cost, first, item) in items {
        let (worker, _) = load
            .iter()
            .enumerate()
            .min_by_key(|(_, load)| **load)
            .unwrap_or((0, &0));
        load[worker] += cost;
        lists[worker].push((first, item));
    }
    lists
        .into_iter()
        .map(|mut list| {
            // Within a worker, keep the planned order.
            list.sort_by_key(|(first, _)| *first);
            let mut text = String::new();
            for (_, item) in list {
                match item {
                    Item::File(entry) => {
                        let _ = writeln!(
                            text,
                            "f {} {} {} {} {} {}",
                            entry.index,
                            entry.offset,
                            entry.size,
                            entry.mtime.as_deref().unwrap_or("-"),
                            u8::from(entry.replace),
                            quote(&entry.active_path)
                        );
                    }
                    Item::Batch(number) => {
                        let _ = writeln!(text, "b {number}");
                    }
                }
            }
            text
        })
        .collect()
}

/// The `stage` file naming the target side's staging folder.
pub(crate) fn stage_file(stage: &str) -> String {
    format!("STAGE={}\n", quote(stage))
}

/// The passive side's lookup tables: `(number, script)` for `tab/<n>.sh`.
pub(crate) fn tables(entries: &[Entry]) -> Vec<(usize, String)> {
    let mut tables: Vec<(usize, String)> = Vec::new();
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by_key(|entry| entry.index);
    for entry in sorted {
        let number = entry.index / TABLE_SIZE;
        if tables.last().is_none_or(|(last, _)| *last != number) {
            if let Some((_, text)) = tables.last_mut() {
                text.push_str("esac\n");
            }
            tables.push((number, "case $1 in\n".to_owned()));
        }
        if let Some((_, text)) = tables.last_mut() {
            let _ = writeln!(
                text,
                "{}) p={} s={} m={} r={} ;;",
                entry.index,
                quote(&entry.passive_path),
                entry.size,
                entry.mtime.as_deref().unwrap_or("-"),
                u8::from(entry.replace),
            );
        }
    }
    if let Some((_, text)) = tables.last_mut() {
        text.push_str("esac\n");
    }
    tables
}

/// A line of the driver's `status` file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Status {
    Begin {
        time: f64,
        pid: u32,
    },
    Connected {
        ok: bool,
        time: f64,
    },
    Started {
        index: usize,
        time: f64,
    },
    BatchStarted {
        batch: usize,
        time: f64,
    },
    Done {
        index: usize,
        bytes: u64,
        time: f64,
    },
    Failed {
        index: usize,
        code: i32,
        time: f64,
        reason: String,
    },
    BatchEnded {
        batch: usize,
        code: i32,
        time: f64,
    },
    End {
        code: i32,
        time: f64,
    },
}

impl Status {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let mut words = line.splitn(5, ' ');
        let kind = words.next()?;
        let mut next = || words.next();
        Some(match kind {
            "B" => Self::Begin {
                time: next()?.parse().ok()?,
                pid: next()?.parse().ok()?,
            },
            "M" => Self::Connected {
                ok: next()? == "ok",
                time: next()?.parse().ok()?,
            },
            "s" => Self::Started {
                index: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
            },
            "S" => Self::BatchStarted {
                batch: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
            },
            "d" => Self::Done {
                index: next()?.parse().ok()?,
                bytes: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
            },
            "e" => Self::Failed {
                index: next()?.parse().ok()?,
                code: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
                reason: next().unwrap_or_default().trim().to_owned(),
            },
            "X" => Self::BatchEnded {
                batch: next()?.parse().ok()?,
                code: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
            },
            "E" => Self::End {
                code: next()?.parse().ok()?,
                time: next()?.parse().ok()?,
            },
            _ => return None,
        })
    }
}

/// A human explanation of a failure reason the scripts report.
pub(crate) fn explain(reason: &str, code: i32) -> String {
    let reason = reason.trim();
    if reason == "exists" {
        return "already exists".to_owned();
    }
    if let Some(size) = reason.strip_prefix("size ") {
        return format!(
            "arrived incomplete ({size} bytes); the source changed or could not be read"
        );
    }
    if let Some(have) = reason.strip_prefix("resume-gap ") {
        return format!("the partial file has only {have} bytes; start the file over");
    }
    match reason {
        "write-failed" => "could not be written on the target".to_owned(),
        "rename-failed" => "could not be renamed into place".to_owned(),
        "truncate-failed" => "the partial file could not be shortened".to_owned(),
        "missing" => "did not arrive; it could not be read on the source".to_owned(),
        "no-stage" | "stage-failed" => {
            "no staging folder could be made next to the targets".to_owned()
        }
        "batch-failed" | "" if code == 255 => {
            "the connection between the servers failed".to_owned()
        }
        "batch-failed" => format!("its batch failed (code {code})"),
        "" => format!("failed (code {code})"),
        other => other.to_owned(),
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::*;

    const NASTY: &[&str] = &[
        "plain.txt",
        "with space and 'quotes' \"double\"",
        "dollar $HOME `id` $(id)",
        "line\nbreak",
        "back\\slash %s %b \\0101",
        "-leading dash",
        "ümlaut 日本 🦀",
        "glob * ? [a-z] ~ ! # ;&|<>",
    ];

    struct Sides {
        _temp: tempfile::TempDir,
        active: PathBuf,
        passive: PathBuf,
        config: Config,
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn config(mode: Mode) -> Config {
        Config {
            job: "j0123456789abcdef".into(),
            mode,
            peer_host: "peer.example".into(),
            peer_port: 22,
            peer_user: "ferge".into(),
            workers: 2,
            keep: false,
            verbose: false,
            linger: 1,
        }
    }

    /// Both job folders, with `peer.sh` replaced by a stub that runs the
    /// passive side's forced command directly.
    fn sides(mode: Mode) -> Sides {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("active");
        let passive = temp.path().join("passive");
        let config = config(mode);
        let active_job = active.join(config.dir());
        let passive_job = passive.join(config.dir());
        write(&active_job.join("lib.sh"), &lib_sh(&config));
        write(&active_job.join("driver.sh"), &driver_sh(&config));
        write(&active_job.join("stop.sh"), &stop_sh(&config));
        let stub = format!(
            "{}\npeer_open() {{ :; }}\npeer_close() {{ :; }}\n\
             peer() {{ (cd {home} && HOME={home} SSH_ORIGINAL_COMMAND=\"$*\" sh {dir}/passive.sh); }}\n\
             peer_once() {{ peer \"$@\"; }}\n",
            peer_sh(&config),
            home = quote(passive.to_str().unwrap()),
            dir = config.dir(),
        );
        write(&active_job.join("peer.sh"), &stub);
        write(&passive_job.join("lib.sh"), &lib_sh(&config));
        write(&passive_job.join("passive.sh"), &passive_sh(&config));
        write(&passive_job.join("install.sh"), &install_sh(&config));
        write(&passive_job.join("uninstall.sh"), &uninstall_sh(&config));
        Sides {
            _temp: temp,
            active,
            passive,
            config,
        }
    }

    fn run(home: &Path, script: &str, args: &[&str]) -> std::process::Output {
        Command::new("sh")
            .arg(script)
            .args(args)
            .current_dir(home)
            .env("HOME", home)
            .output()
            .unwrap()
    }

    /// Runs a `sh -s` script with `home` as the login directory.
    fn run_stdin(home: &Path, script: &str) -> String {
        use std::io::Write as _;
        let mut child = Command::new("sh")
            .arg("-s")
            .current_dir(home)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn status(sides: &Sides) -> Vec<Status> {
        let text =
            std::fs::read_to_string(sides.active.join(sides.config.dir()).join("status")).unwrap();
        text.lines()
            .map(|line| Status::parse(line).unwrap_or_else(|| panic!("{line:?}")))
            .collect()
    }

    /// Uploads what ferge would: lists, tables, batch files, stage.
    fn setup_files(sides: &Sides, entries: Vec<Entry>, batching: bool, stage: &Path) {
        let work = split_work(entries, 2, batching, sides.config.mode);
        let active = sides.active.join(sides.config.dir());
        let passive = sides.passive.join(sides.config.dir());
        for (worker, list) in worker_lists(&work, 2).iter().enumerate() {
            write(&active.join(format!("list.{worker}")), list);
        }
        for (number, table) in tables(&work.singles) {
            write(&passive.join(format!("tab/{number}.sh")), &table);
        }
        for (number, batch) in work.batches.iter().enumerate() {
            write(
                &active.join(format!("batch/{number}.sh")),
                &batch_file(batch, sides.config.mode, Role::Active),
            );
            write(
                &passive.join(format!("batch/{number}.sh")),
                &batch_file(batch, sides.config.mode, Role::Passive),
            );
        }
        let target = match sides.config.target_role() {
            Role::Active => &active,
            Role::Passive => &passive,
        };
        write(
            &target.join("stage"),
            &stage_file(
                stage
                    .join(format!(".ferge-{}", sides.config.job))
                    .to_str()
                    .unwrap(),
            ),
        );
    }

    fn copy_all(mode: Mode, batching: bool) {
        let sides = sides(mode);
        let (source_dir, target_dir) = match mode {
            Mode::Push => (sides.active.join("src"), sides.passive.join("dst")),
            Mode::Pull => (sides.passive.join("src"), sides.active.join("dst")),
        };
        std::fs::create_dir_all(&target_dir).unwrap();
        let mut entries = Vec::new();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_979_045);
        for (index, name) in NASTY.iter().enumerate() {
            let contents = format!("contents of {name}\n").repeat(index * 100 + 1);
            write(&source_dir.join(name), &contents);
            std::fs::File::options()
                .write(true)
                .open(source_dir.join(name))
                .unwrap()
                .set_modified(modified)
                .unwrap();
            let source = source_dir.join(name).to_str().unwrap().to_owned();
            let target = target_dir.join(name).to_str().unwrap().to_owned();
            let (active_path, passive_path) = match mode {
                Mode::Push => (source, target),
                Mode::Pull => (target, source),
            };
            entries.push(Entry {
                index: index * 700, // spans several tables
                offset: 0,
                size: contents.len() as u64,
                mtime: Some("202001021530.45".into()),
                replace: false,
                active_path,
                passive_path,
            });
        }
        setup_files(&sides, entries, batching, &target_dir);

        let driver = format!("{}/driver.sh", sides.config.dir());
        let probe = run(&sides.active, &driver, &["probe"]);
        let printed = String::from_utf8_lossy(&probe.stdout);
        assert!(probe.status.success(), "{printed}");
        assert!(
            printed.contains("peer-reply ferge-passive j0123456789abcdef tar=/"),
            "{printed}"
        );

        let output = run(&sides.active, &driver, &["run"]);
        assert!(output.status.success(), "{output:?}");
        let status = status(&sides);
        let done = status
            .iter()
            .filter(|line| matches!(line, Status::Done { .. }))
            .count();
        assert_eq!(done, NASTY.len(), "{status:?}");
        let batches = status
            .iter()
            .filter(|line| matches!(line, Status::BatchEnded { code: 0, .. }))
            .count();
        assert_eq!(batches > 0, batching, "{status:?}");
        assert!(matches!(status.last(), Some(Status::End { code: 0, .. })));
        for name in NASTY {
            assert_eq!(
                std::fs::read(source_dir.join(name)).unwrap(),
                std::fs::read(target_dir.join(name)).unwrap(),
                "{name:?}"
            );
            let modified = std::fs::metadata(target_dir.join(name))
                .unwrap()
                .modified()
                .unwrap();
            let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_979_045);
            assert_eq!(modified, expected, "{name:?}");
            assert!(!target_dir.join(format!("{name}.ferge-part")).exists());
        }
        // Only the copied files are in the target: the staging folder is gone.
        assert_eq!(std::fs::read_dir(&target_dir).unwrap().count(), NASTY.len());
        // `cleanup` removed the passive job folder; the key file is gone.
        assert!(!sides.passive.join(sides.config.dir()).exists());
        assert!(!sides.active.join(sides.config.dir()).join("key").exists());
    }

    #[test]
    fn pushes_files_with_any_name() {
        copy_all(Mode::Push, false);
    }

    #[test]
    fn pulls_files_with_any_name() {
        copy_all(Mode::Pull, false);
    }

    #[test]
    fn pushes_batches_of_files_with_any_name() {
        copy_all(Mode::Push, true);
    }

    #[test]
    fn pulls_batches_of_files_with_any_name() {
        copy_all(Mode::Pull, true);
    }

    #[test]
    fn resumes_refuses_existing_files_and_detects_short_ones() {
        for batching in [false, true] {
            let sides = sides(Mode::Push);
            let src = sides.active.join("src");
            let dst = sides.passive.join("dst");
            write(&src.join("resumed"), "0123456789");
            write(&dst.join("resumed.ferge-part"), "01234XXXXXXX");
            write(&src.join("taken"), "new");
            write(&dst.join("taken"), "old");
            write(&src.join("replaced"), "new");
            write(&dst.join("replaced"), "old");
            write(&src.join("changed"), "short");
            let entry = |index, name: &str, offset, size, replace| Entry {
                index,
                offset,
                size,
                mtime: None,
                replace,
                active_path: src.join(name).to_str().unwrap().to_owned(),
                passive_path: dst.join(name).to_str().unwrap().to_owned(),
            };
            let entries = vec![
                entry(0, "resumed", 5, 10, false),
                entry(1, "taken", 0, 3, false),
                entry(2, "replaced", 0, 3, true),
                entry(3, "changed", 0, 99, false),
                entry(4, "gone", 0, 4, false),
            ];
            setup_files(&sides, entries, batching, &dst);
            let output = run(
                &sides.active,
                &format!("{}/driver.sh", sides.config.dir()),
                &["run"],
            );
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                std::fs::read_to_string(dst.join("resumed")).unwrap(),
                "0123456789"
            );
            assert_eq!(std::fs::read_to_string(dst.join("taken")).unwrap(), "old");
            assert_eq!(
                std::fs::read_to_string(dst.join("replaced")).unwrap(),
                "new"
            );
            assert!(!dst.join("changed").exists());
            let mut failed: Vec<(usize, String)> = status(&sides)
                .into_iter()
                .filter_map(|line| match line {
                    Status::Failed { index, reason, .. } => Some((index, reason)),
                    _ => None,
                })
                .collect();
            failed.sort();
            let gone = if batching { "missing" } else { "size 0 of 4" };
            assert_eq!(
                failed,
                vec![
                    (1, "exists".to_owned()),
                    (3, "size 5 of 99".to_owned()),
                    (4, gone.to_owned())
                ],
                "batching: {batching}"
            );
        }
    }

    #[test]
    fn copies_linked_files_of_a_batch_apart() {
        use std::os::unix::fs::MetadataExt as _;
        let sides = sides(Mode::Push);
        let src = sides.active.join("src");
        let dst = sides.passive.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        write(&src.join("one"), "same");
        std::fs::hard_link(src.join("one"), src.join("two")).unwrap();
        write(&src.join("sub/three"), "other");
        let entry = |index, name: &str| Entry {
            index,
            offset: 0,
            size: std::fs::metadata(src.join(name)).unwrap().len(),
            mtime: None,
            replace: false,
            active_path: src.join(name).to_str().unwrap().to_owned(),
            passive_path: dst.join(name).to_str().unwrap().to_owned(),
        };
        std::fs::create_dir_all(dst.join("sub")).unwrap();
        setup_files(
            &sides,
            vec![entry(0, "one"), entry(1, "two"), entry(2, "sub/three")],
            true,
            &dst,
        );
        let output = run(
            &sides.active,
            &format!("{}/driver.sh", sides.config.dir()),
            &["run"],
        );
        assert!(output.status.success(), "{output:?}");
        let done = status(&sides)
            .into_iter()
            .filter(|line| matches!(line, Status::Done { .. }))
            .count();
        assert_eq!(done, 3);
        let inode = |name: &str| std::fs::metadata(dst.join(name)).unwrap().ino();
        assert_ne!(inode("one"), inode("two"));
        assert_eq!(std::fs::metadata(dst.join("one")).unwrap().nlink(), 1);
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/three")).unwrap(),
            "other"
        );
    }

    #[test]
    fn rejects_requests_outside_the_job() {
        let sides = sides(Mode::Push);
        setup_files(&sides, Vec::new(), true, &sides.passive);
        for request in [
            "put 0 0",
            "get ../x 0",
            "put 1;id 0",
            "rm -rf /",
            "get 5 x",
            "putb 0",
            "getb ../../x",
            "putb 1;id",
        ] {
            let output = Command::new("sh")
                .arg(format!("{}/passive.sh", sides.config.dir()))
                .current_dir(&sides.passive)
                .env("HOME", &sides.passive)
                .env("SSH_ORIGINAL_COMMAND", request)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(!output.status.success(), "{request}");
        }
    }

    /// Everything below `home`: path, contents (files) and modification
    /// time, to compare before and after.
    fn snapshot(home: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, std::time::SystemTime)> {
        let mut all = Vec::new();
        let mut stack = vec![home.to_path_buf()];
        while let Some(path) = stack.pop() {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let contents = if metadata.is_dir() {
                for entry in std::fs::read_dir(&path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
                None
            } else {
                Some(std::fs::read(&path).unwrap())
            };
            all.push((path, contents, metadata.modified().unwrap()));
        }
        all.sort();
        all
    }

    /// Sets up a job on `home`, installs its key and removes it all again.
    fn install_and_remove(home: &Path) -> (String, String) {
        let config = config(Mode::Push);
        let setup = run_stdin(home, &setup_script(&config));
        assert!(
            setup.contains("\nready\n") && setup.contains("zone "),
            "{setup}"
        );
        let job = home.join(config.dir());
        write(&job.join("lib.sh"), &lib_sh(&config));
        write(&job.join("install.sh"), &install_sh(&config));
        write(&job.join("uninstall.sh"), &uninstall_sh(&config));
        let line = authorized_key(&config, "ssh-ed25519 AAAAjob comment", "209912312359");
        write(&job.join("authorized_key"), &line);
        let output = run(home, &format!("{}/install.sh", config.dir()), &[]);
        assert!(output.status.success(), "{output:?}");
        let installed = String::from_utf8(output.stdout).unwrap();
        assert!(installed.contains("installed"), "{installed}");
        let keys = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        let output = run(home, &format!("{}/uninstall.sh", config.dir()), &[]);
        assert!(output.status.success(), "{output:?}");
        let finished = run_stdin(
            home,
            &finish_script(
                &config,
                &Finish {
                    stage: None,
                    unkey: true,
                    keep: false,
                },
            ),
        );
        assert!(!finished.contains("left "), "{finished}");
        (keys, setup)
    }

    #[test]
    fn leaves_a_home_as_it_was() {
        // A key file without a final newline, among other files.
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write(
            &home.join(".ssh/authorized_keys"),
            "ssh-ed25519 AAAAother someone",
        );
        write(&home.join(".ssh/config"), "Host *\n");
        write(&home.join(".cache/other/file"), "x");
        write(&home.join("notes.txt"), "hello");
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_mins(25_000_000);
        for path in [".ssh/authorized_keys", ".ssh", ".cache", ""] {
            let file = std::fs::File::open(home.join(path)).unwrap();
            file.set_modified(old).unwrap();
        }
        let before = snapshot(home);
        let (keys, _) = install_and_remove(home);
        assert!(
            keys.starts_with(
                "ssh-ed25519 AAAAother someone\nrestrict,expiry-time=\"209912312359\","
            ) && keys.ends_with(" ferge-job-j0123456789abcdef made=newline\n"),
            "{keys}"
        );
        assert_eq!(snapshot(home), before);
    }

    #[test]
    fn removes_what_it_made_in_an_empty_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_mins(25_000_000);
        std::fs::File::open(home)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = snapshot(home);
        let (keys, setup) = install_and_remove(home);
        assert!(keys.ends_with(" made=dir,file\n"), "{keys}");
        assert!(setup.contains("note ssh -") && setup.contains("note cache -"));
        assert_eq!(snapshot(home), before);
    }

    #[test]
    fn sweeps_what_earlier_jobs_left() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write(
            &home.join(".ssh/authorized_keys"),
            "ssh-ed25519 AAAAother someone\n",
        );
        // A key whose job folder is gone, one that expired long ago, and one
        // of a job that is still being answered.
        let mut keys = String::from("ssh-ed25519 AAAAother someone\n");
        for (job, expiry) in [
            ("j1111111111111111", "209912312359"),
            ("j2222222222222222", "200001010000"),
            ("j3333333333333333", "209912312359"),
        ] {
            let _ = writeln!(
                keys,
                "restrict,expiry-time=\"{expiry}\",command=\"sh x\" ssh-ed25519 AAAA{job} ferge-job-{job}"
            );
        }
        write(&home.join(".ssh/authorized_keys"), &keys);
        write(
            &home.join(".cache/ferge/jobs/j2222222222222222/passive.sh"),
            "",
        );
        write(
            &home.join(".cache/ferge/jobs/j3333333333333333/passive.sh"),
            "",
        );
        // A job that ended without ferge and kept its result.
        write(
            &home.join(".cache/ferge/jobs/j4444444444444444/result"),
            "job j4444444444444444 ended without ferge\nE 0 12\n",
        );
        let swept = run_stdin(home, &stdin_script(Some("j9999999999999999"), "sweep\n"));
        assert!(swept.contains("swept key j1111111111111111"), "{swept}");
        assert!(swept.contains("swept key j2222222222222222"), "{swept}");
        assert!(!swept.contains("j3333333333333333"), "{swept}");
        assert!(swept.contains("result j4444444444444444\n| job j4444444444444444"));
        assert!(swept.contains("swept job j4444444444444444"), "{swept}");
        let keys = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert!(keys.starts_with("ssh-ed25519 AAAAother someone\n"));
        assert!(keys.contains("ferge-job-j3333333333333333") && keys.lines().count() == 2);
        assert!(!home.join(".cache/ferge/jobs/j4444444444444444").exists());

        let purged = run_stdin(home, &purge_script());
        assert!(purged.contains("purged"), "{purged}");
        assert_eq!(
            std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap(),
            "ssh-ed25519 AAAAother someone\n"
        );
        assert!(!home.join(".cache/ferge").exists());
        let inspected = run_stdin(home, &inspect_script());
        assert_eq!(inspected, "inspected\n");
    }

    #[test]
    fn renews_only_its_own_key() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let config = config(Mode::Push);
        let line = authorized_key(&config, "ssh-ed25519 AAAAjob comment", "202001010000");
        let other = "restrict,expiry-time=\"202001010000\" ssh-ed25519 AAAAx other\n";
        write(
            &home.join(".ssh/authorized_keys"),
            &format!("{other}{line}"),
        );
        let renewed = run_stdin(home, &renew_script(&config.job, "209901010000"));
        assert!(renewed.contains("renewed"), "{renewed}");
        let keys = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(
            keys,
            format!("{other}{}", line.replace("202001010000", "209901010000"))
        );
    }

    #[test]
    fn batches_small_files_and_spreads_the_work() {
        let entry = |index, size, offset| Entry {
            index,
            offset,
            size,
            mtime: None,
            replace: false,
            active_path: format!("/src/f{index}"),
            passive_path: format!("/dst/f{index}"),
        };
        let mut entries = vec![
            entry(0, 100 << 20, 0),
            entry(1, 100 << 20, 0),
            entry(2, 9, 3),
        ];
        entries.extend((3..103).map(|index| entry(index, 10, 0)));
        let work = split_work(entries, 2, true, Mode::Push);
        assert_eq!(work.singles.len(), 3);
        assert_eq!(work.batches.iter().map(Vec::len).sum::<usize>(), 100);
        assert!(work.batches.iter().all(|batch| batch.len() <= 16));
        let lists = worker_lists(&work, 2);
        assert_eq!(lists.len(), 2);
        assert!(lists[0].starts_with("f 0 "), "{}", lists[0]);
        assert!(lists[1].starts_with("f 1 "), "{}", lists[1]);
        assert!(lists.iter().all(|list| list.contains("\nb ")), "{lists:?}");
        assert_eq!(lists.concat().matches("f 2 ").count(), 1);
        let unbatched = split_work(vec![entry(0, 10, 0)], 2, false, Mode::Push);
        assert!(unbatched.batches.is_empty() && unbatched.singles.len() == 1);
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(
            Status::parse("e 7 1 1700000000.25 size 3 of 9"),
            Some(Status::Failed {
                index: 7,
                code: 1,
                time: 1_700_000_000.25,
                reason: "size 3 of 9".into()
            })
        );
        assert_eq!(
            Status::parse("d 7 99 12"),
            Some(Status::Done {
                index: 7,
                bytes: 99,
                time: 12.0
            })
        );
        assert_eq!(
            Status::parse("X 3 255 12.5"),
            Some(Status::BatchEnded {
                batch: 3,
                code: 255,
                time: 12.5
            })
        );
        assert_eq!(Status::parse("x y"), None);
        assert!(is_plain("example.org") && is_plain("[::1]") && !is_plain("-oProxy"));
        assert!(!is_plain("a b") && !is_plain("a;b"));
    }
}
