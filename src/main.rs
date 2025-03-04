use clap::{value_parser, Arg, ArgAction, Command};
use nix::errno::Errno;
use nix::sys::time::{TimeSpec, TimeValLike};
use nix::time::{clock_nanosleep, ClockId, ClockNanosleepFlags};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use thread_priority::ScheduleParams;
use thread_priority::{
    unix::{
        set_thread_priority_and_policy, thread_native_id, RealtimeThreadSchedulePolicy,
        ThreadSchedulePolicy,
    },
    ThreadPriority,
};

struct Statistics {
    min_jitter: i64,
    max_jitter: i64,
    avg_jitter: f64,
    total_jitter: i64,
    cycles: u64,
}

struct SharedData {
    stats: Mutex<Statistics>,
    running: AtomicBool,
    nanoseconds: bool,
}

fn main() {
    let matches = Command::new("latency")
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
                .help("Priority of the threads (default: 1)")
                .action(ArgAction::Set)
                .default_value("1"),
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
            min_jitter: i64::MAX,
            max_jitter: i64::MIN,
            avg_jitter: 0.0,
            total_jitter: 0,
            cycles: 0,
        }),
        running: AtomicBool::new(true),
        nanoseconds,
    });

    let shared_data_clone = shared_data.clone();

    let (tx, rx): (Sender<()>, Receiver<()>) = channel();

    ctrlc::set_handler(move || {
        println!("Ctrl-C received, stopping threads...");
        shared_data_clone.running.store(false, Ordering::SeqCst);
        let _ = tx.send(());
    })
    .expect("Error setting Ctrl-C handler");

    for i in 0..num_threads {
        let shared_data_thread = shared_data.clone();
        println!(
            "Starting thread {} with interval {}us and priority {}",
            i, interval_us, priority
        );
        let _handle = thread::spawn(move || {
            run_thread(shared_data_thread, interval_us, priority);
        });
    }

    let shared_data_report = shared_data.clone();
    report_statistics(shared_data_report, rx);
}

fn run_thread(shared_data: Arc<SharedData>, interval_us: u64, priority: i32) {
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

    let start = ClockId::CLOCK_MONOTONIC.now().unwrap();
    let interval = TimeSpec::from_duration(Duration::from_micros(interval_us));
    let mut next_wakeup = start + interval;
    while shared_data.running.load(Ordering::SeqCst) {
        if let Err(e) = clock_nanosleep(
            ClockId::CLOCK_MONOTONIC,
            ClockNanosleepFlags::TIMER_ABSTIME,
            &next_wakeup,
        ) {
            if e != Errno::EINTR {
                eprintln!("clock_nanosleep failed: {}", e);
                break;
            }
        }
        let actual_wakeup = ClockId::CLOCK_MONOTONIC.now().unwrap();
        let jitter = actual_wakeup.num_nanoseconds() - next_wakeup.num_nanoseconds();
        next_wakeup = next_wakeup + interval;

        let mut stats = shared_data.stats.lock().unwrap();
        stats.cycles += 1;
        stats.total_jitter += jitter;
        stats.min_jitter = stats.min_jitter.min(jitter);
        stats.max_jitter = stats.max_jitter.max(jitter);
        stats.avg_jitter = stats.total_jitter as f64 / stats.cycles as f64;
    }
}

fn report_statistics(shared_data: Arc<SharedData>, rx: Receiver<()>) {
    loop {
        match rx.try_recv() {
            Ok(_) => break,
            Err(_) => {
                let stats = shared_data.stats.lock().unwrap();
                if shared_data.nanoseconds {
                    println!("Min jitter: {} ns", stats.min_jitter);
                    println!("Max jitter: {} ns", stats.max_jitter);
                    println!("Avg jitter: {:.2} ns", stats.avg_jitter);
                } else {
                    println!("Min jitter: {} us", stats.min_jitter / 1000);
                    println!("Max jitter: {} us", stats.max_jitter / 1000);
                    println!("Avg jitter: {:.2} us", stats.avg_jitter / 1000.0);
                }
                println!("Cycles: {}", stats.cycles);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}
