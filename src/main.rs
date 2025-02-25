use clap::{value_parser, Arg, ArgAction, Command};
use nix::errno::Errno;
use nix::sched::{sched_setaffinity, CpuSet};
use nix::time::{clock_nanosleep, ClockId, ClockNanosleepFlags};
use nix::unistd::Pid;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thread_priority::ScheduleParams;
use thread_priority::{
    unix::{
        set_thread_priority_and_policy, thread_native_id, RealtimeThreadSchedulePolicy,
        ThreadSchedulePolicy,
    },
    ThreadPriority,
};

struct Statistics {
    min_latency: i64,
    max_latency: i64,
    avg_latency: f64,
    total_latency: i64,
    cycles: u64,
}

struct SharedData {
    stats: Mutex<Statistics>,
    running: AtomicBool,
    nanoseconds: bool,
}

fn main() {
    let matches = Command::new("simple_cyclictest")
        .version("1.0")
        .author("Sirius Wu")
        .about("Simplified cyclictest in Rust")
        .arg(
            Arg::new("threads")
                .value_parser(value_parser!(i32))
                .short('t')
                .long("threads")
                .value_name("NUM")
                .help("Number of threads (default: 1)")
                .action(ArgAction::Set)
                .default_value("1"),
        )
        .arg(
            Arg::new("interval")
                .value_parser(value_parser!(u64))
                .short('i')
                .long("interval")
                .value_name("INTV")
                .help("Interval in microseconds (default: 1000)")
                .action(ArgAction::Set)
                .default_value("1000"),
        )
        .arg(
            Arg::new("priority")
                .value_parser(value_parser!(i32))
                .short('p')
                .long("priority")
                .value_name("PRIO")
                .help("Priority of the threads (default: 50)")
                .action(ArgAction::Set)
                .default_value("50"),
        )
        .arg(
            Arg::new("cpu")
                .short('a')
                .long("cpu")
                .value_name("CPU")
                .help("CPU affinity (optional)")
                .action(ArgAction::Set)
                .default_value(None),
        )
        .arg(
            Arg::new("nanoseconds")
                .short('N')
                .long("nanoseconds")
                .help("Print results in nanoseconds instead of microseconds"),
        )
        .arg(
            Arg::new("mlock")
                .short('m')
                .long("mlock")
                .help("Lock current and future memory allocations"),
        )
        .get_matches();

    let num_threads = *matches.get_one("threads").or(Some(&1)).expect("threads");
    let interval_us = *matches
        .get_one("interval")
        .or(Some(&1000u64))
        .expect("interval");
    let priority = *matches.get_one("priority").or(Some(&50)).expect("priority");
    let cpu_affinity: Option<usize> = matches.get_one::<usize>("cpu").copied();
    let nanoseconds = matches.contains_id("nanoseconds");
    let mlock = matches.contains_id("mlock");

    if mlock {
        if let Err(e) = nix::sys::mman::mlockall(
            nix::sys::mman::MlockAllFlags::MCL_CURRENT | nix::sys::mman::MlockAllFlags::MCL_FUTURE,
        ) {
            eprintln!("Failed to mlockall: {}", e);
        }
    }

    let shared_data = Arc::new(SharedData {
        stats: Mutex::new(Statistics {
            min_latency: i64::MAX,
            max_latency: i64::MIN,
            avg_latency: 0.0,
            total_latency: 0,
            cycles: 0,
        }),
        running: AtomicBool::new(true),
        nanoseconds,
    });

    let shared_data_clone = shared_data.clone();

    let (tx, rx): (Sender<()>, Receiver<()>) = channel();

    ctrlc::set_handler(move || {
        shared_data_clone.running.store(false, Ordering::SeqCst);
        let _ = tx.send(());
    })
    .expect("Error setting Ctrl-C handler");

    let mut threads = Vec::new();

    for _ in 0..num_threads {
        let shared_data_thread = shared_data.clone();
        let handle = thread::spawn(move || {
            run_thread(shared_data_thread, interval_us, priority, cpu_affinity);
        });
        threads.push(handle);
    }

    let shared_data_report = shared_data.clone();
    thread::spawn(move || {
        report_statistics(shared_data_report, rx);
    });

    for handle in threads {
        handle.join().unwrap();
    }
}

fn run_thread(
    shared_data: Arc<SharedData>,
    interval_us: u64,
    priority: i32,
    cpu_affinity: Option<usize>,
) {
    let thread_id = thread_native_id();
    let params = ScheduleParams {
        sched_priority: priority,
    };
    let priority = ThreadPriority::from_posix(params);
    if let Err(e) = set_thread_priority_and_policy(
        thread_id,
        priority,
        ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
    ) {
        eprintln!("Failed to set scheduler: {}", e);
        return;
    }

    if let Some(cpu) = cpu_affinity {
        let mut cpu_set = CpuSet::new();
        let pid = Pid::this();
        cpu_set.set(cpu).unwrap();
        if let Err(e) = sched_setaffinity(pid, &cpu_set) {
            eprintln!("Failed to set affinity: {}", e);
            return;
        }
    }

    let mut next_wakeup = SystemTime::now();

    while shared_data.running.load(Ordering::SeqCst) {
        next_wakeup += Duration::from_micros(interval_us);
        let duration_since_epoch = next_wakeup
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards");
        let timespec = nix::sys::time::TimeSpec::from(duration_since_epoch);

        let start = Instant::now();
        if let Err(e) = clock_nanosleep(
            ClockId::CLOCK_MONOTONIC,
            ClockNanosleepFlags::TIMER_ABSTIME,
            &timespec,
        ) {
            if e != Errno::EINTR {
                eprintln!("clock_nanosleep failed: {}", e);
                break;
            }
        }
        let actual_wakeup = Instant::now();
        let latency = actual_wakeup.duration_since(start).as_micros() as i64;

        let mut stats = shared_data.stats.lock().unwrap();
        stats.cycles += 1;
        stats.total_latency += latency;
        stats.min_latency = stats.min_latency.min(latency);
        stats.max_latency = stats.max_latency.max(latency);
        stats.avg_latency = stats.total_latency as f64 / stats.cycles as f64;
    }
}

fn report_statistics(shared_data: Arc<SharedData>, rx: Receiver<()>) {
    loop {
        match rx.try_recv() {
            Ok(_) => break,
            Err(_) => {
                let stats = shared_data.stats.lock().unwrap();
                if shared_data.nanoseconds {
                    println!("Min latency: {} ns", stats.min_latency * 1000);
                    println!("Max latency: {} ns", stats.max_latency * 1000);
                    println!("Avg latency: {:.2} ns", stats.avg_latency * 1000.0);
                } else {
                    println!("Min latency: {} us", stats.min_latency);
                    println!("Max latency: {} us", stats.max_latency);
                    println!("Avg latency: {:.2} us", stats.avg_latency);
                }
                println!("Cycles: {}", stats.cycles);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}
