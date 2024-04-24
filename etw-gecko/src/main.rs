use std::{collections::{HashMap, HashSet, hash_map::Entry, VecDeque}, convert::TryInto, fs::File, io::BufWriter, path::Path, time::{Duration, Instant, SystemTime}, sync::Arc};

use context_switch::{OffCpuSampleGroup, ThreadContextSwitchData};
use etw_reader::{GUID, open_trace, parser::{Parser, TryParse, Address}, print_property, schema::SchemaLocator, write_property};
use lib_mappings::{LibMappingOpQueue, LibMappingOp, LibMappingAdd};
use serde_json::{Value, json, to_writer};
use fxprof_processed_profile::{debugid, CategoryColor, CategoryHandle, CategoryPairHandle, CounterHandle, CpuDelta, FrameFlags, FrameInfo, LibraryHandle, LibraryInfo, MarkerDynamicField, MarkerFieldFormat, MarkerLocation, MarkerSchema, MarkerSchemaField, MarkerTiming, ProcessHandle, Profile, ProfilerMarker, ReferenceTimestamp, SamplingInterval, Symbol, SymbolTable, ThreadHandle, Timestamp};
use debugid::DebugId;
use bitflags::bitflags;


mod context_switch;
mod jit_category_manager;
mod jit_function_add_marker;
mod lib_mappings;
mod marker_file;
mod process_sample_data;
mod stack_converter;
mod stack_depth_limiting_frame_iter;
mod timestamp_converter;
mod types;
mod unresolved_samples;

use jit_category_manager::JitCategoryManager;
use stack_converter::StackConverter;
use lib_mappings::LibMappingInfo;
use types::{StackFrame, StackMode};
use unresolved_samples::{UnresolvedSamples, UnresolvedStacks};
use uuid::Uuid;
use process_sample_data::ProcessSampleData;

use crate::{context_switch::ContextSwitchHandler, jit_function_add_marker::JitFunctionAddMarker, marker_file::get_markers, process_sample_data::UserTimingMarker, timestamp_converter::TimestampConverter};

/// An example marker type with some text content.
#[derive(Debug, Clone)]
pub struct TextMarker(pub String);

impl ProfilerMarker for TextMarker {
    const MARKER_TYPE_NAME: &'static str = "Text";

    fn json_marker_data(&self) -> serde_json::Value {
        json!({
            "type": Self::MARKER_TYPE_NAME,
            "name": self.0
        })
    }

    fn schema() -> MarkerSchema {
        MarkerSchema {
            type_name: Self::MARKER_TYPE_NAME,
            locations: vec![MarkerLocation::MarkerChart, MarkerLocation::MarkerTable],
            chart_label: Some("{marker.data.name}"),
            tooltip_label: Some("{marker.data.name}"),
            table_label: Some("{marker.name} - {marker.data.name}"),
            fields: vec![MarkerSchemaField::Dynamic(MarkerDynamicField {
                key: "name",
                label: "Name",
                format: MarkerFieldFormat::String,
                searchable: true,
            })],
        }
    }
}

fn is_kernel_address(ip: u64, pointer_size: u32) -> bool {
    if pointer_size == 4 {
        return ip >= 0x80000000;
    }
    return ip >= 0xFFFF000000000000;        // TODO I don't know what the true cutoff is.
}

fn stack_mode_for_address(address: u64, pointer_size: u32) -> StackMode {
    if is_kernel_address(address, pointer_size) {
        StackMode::Kernel
    } else {
        StackMode::User
    }
}

/// An on- or off-cpu-sample for which the user stack is not known yet.
/// Consumed once the user stack arrives.
#[derive(Debug, Clone)]
struct PendingStack {
    /// The timestamp of the SampleProf or CSwitch event
    timestamp: u64,
    /// Starts out as None. Once we encounter the kernel stack (if any), we put it here.
    kernel_stack: Option<Vec<StackFrame>>,
    off_cpu_sample_group: Option<OffCpuSampleGroup>,
    on_cpu_sample_cpu_delta: Option<CpuDelta>,
}

struct PendingMarker {
    text: String,
    start: Timestamp,
}

struct ThreadState {
    // When merging threads `handle` is the global thread handle and we use `merge_name` to store the name
    handle: ThreadHandle,
    merge_name: Option<String>,
    pending_stacks: VecDeque<PendingStack>,
    pending_markers: HashMap<String, PendingMarker>,
    context_switch_data: ThreadContextSwitchData,
    thread_id: u32
}

impl ThreadState {
    fn new(handle: ThreadHandle, tid: u32) -> Self {
        ThreadState {
            handle,
            pending_stacks: VecDeque::new(),
            pending_markers: HashMap::new(),
            context_switch_data: ThreadContextSwitchData::default(),
            merge_name: None,
            thread_id: tid
        }
    }
}


fn strip_thread_numbers(name: &str) -> &str {
    if let Some(hash) = name.find('#') {
        let (prefix, suffix) = name.split_at(hash);
        if suffix[1..].parse::<i32>().is_ok() {
            return prefix.trim();
        }
    }
    return name;
}

struct MemoryUsage {
    counter: CounterHandle,
    value: f64
}

struct ProcessJitInfo {
    lib_handle: LibraryHandle,
    jit_mapping_ops: LibMappingOpQueue,
    next_relative_address: u32,
    symbols: Vec<Symbol>,
}

struct ProcessState {
    process_handle: ProcessHandle,
    unresolved_samples: UnresolvedSamples,
    regular_lib_mapping_ops: LibMappingOpQueue,
    main_thread_handle: Option<ThreadHandle>,
    pending_libraries: HashMap<u64, LibraryInfo>,
}

impl ProcessState {
    pub fn new(process_handle: ProcessHandle) -> Self {
        Self {
            process_handle,
            unresolved_samples: UnresolvedSamples::default(),
            regular_lib_mapping_ops: LibMappingOpQueue::default(),
            main_thread_handle: None,
            pending_libraries: HashMap::new(),
        }
    }
}

fn main() {
    let profile_start_instant = Timestamp::from_nanos_since_reference(0);
    let profile_start_system = SystemTime::now();

    let mut schema_locator = SchemaLocator::new();
    etw_reader::add_custom_schemas(&mut schema_locator);
    let mut threads: HashMap<u32, ThreadState> = HashMap::new();
    let mut processes: HashMap<u32, ProcessState> = HashMap::new();
    let mut kernel_pending_libraries: HashMap<u64, LibraryInfo> = HashMap::new();
    let mut memory_usage: HashMap<u32, MemoryUsage> = HashMap::new();

    let mut libs: HashMap<u64, (String, u32, u32)> = HashMap::new();
    let start = Instant::now();
    let mut pargs = pico_args::Arguments::from_env();
    let merge_threads = pargs.contains("--merge-threads");
    let include_idle = pargs.contains("--idle");
    let demand_zero_faults = pargs.contains("--demand-zero-faults");
    let marker_file: Option<String> = pargs.opt_value_from_str("--marker-file").unwrap();
    let marker_prefix: Option<String> = pargs.opt_value_from_str("--filter-by-marker-prefix").unwrap();

    let trace_file: String = pargs.free_from_str().unwrap();

    let mut process_targets = HashSet::new();
    let mut process_target_name = None;
    if let Ok(process_filter) = pargs.free_from_str::<String>() {
        if let Ok(process_id) = process_filter.parse() {
            process_targets.insert(process_id);
        } else {
            println!("targeting {}", process_filter);
            process_target_name = Some(process_filter);
        }
    } else {
        println!("No process specified");
        std::process::exit(1);
    }
    
    let command_name = process_target_name.as_deref().unwrap_or("firefox");
    let mut profile = Profile::new(command_name, ReferenceTimestamp::from_system_time(profile_start_system),  SamplingInterval::from_nanos(122100)); // 8192Hz

    let user_category: CategoryPairHandle = profile.add_category("User", fxprof_processed_profile::CategoryColor::Yellow).into();
    let kernel_category: CategoryPairHandle = profile.add_category("Kernel", fxprof_processed_profile::CategoryColor::Orange).into();

    let mut jit_category_manager = JitCategoryManager::new();
    let mut unresolved_stacks = UnresolvedStacks::default();
    let mut context_switch_handler = ContextSwitchHandler::new(122100);

    let mut thread_index = 0;
    let mut sample_count = 0;
    let mut stack_sample_count = 0;
    let mut dropped_sample_count = 0;
    let mut timer_resolution: u32 = 0; // Resolution of the hardware timer, in units of 100 nanoseconds.
    let mut event_count = 0;
    let (global_thread, global_process) = if merge_threads {
        let global_process = profile.add_process("All processes", 1, profile_start_instant);
        (Some(profile.add_thread(global_process, 1, profile_start_instant, true)), Some(global_process))
    } else {
        (None, None)
    };
    let mut gpu_thread = None;
    let mut jscript_symbols: HashMap<u32, ProcessJitInfo> = HashMap::new();
    let mut jscript_sources: HashMap<u64, String> = HashMap::new();

    // Make a dummy TimestampConverter. Once we've parsed the header, this will have correct values.
    let mut timestamp_converter = TimestampConverter {
        reference_raw: 0,
        raw_to_ns_factor: 1,
    };
    let mut event_timestamps_are_qpc = false;

    let mut categories = HashMap::<String, CategoryHandle>::new();
    let result = open_trace(Path::new(&trace_file), |e| {
        event_count += 1;
        let s = schema_locator.event_schema(e);
        if let Ok(s) = s {
            match s.name() {
                "MSNT_SystemTrace/EventTrace/Header" => {
                    let mut parser = Parser::create(&s);
                    timer_resolution = parser.parse("TimerResolution");
                    let perf_freq: u64 = parser.parse("PerfFreq");
                    let clock_type: u32 = parser.parse("ReservedFlags");
                    if clock_type != 1 {
                        println!("WARNING: QPC not used as clock");
                        event_timestamps_are_qpc = false;
                    } else {
                        event_timestamps_are_qpc = true;
                    }
                    let events_lost: u32 = parser.parse("EventsLost");
                    if events_lost != 0 {
                        println!("WARNING: {} events lost", events_lost);
                    }

                    timestamp_converter = TimestampConverter {
                        reference_raw: e.EventHeader.TimeStamp as u64,
                        raw_to_ns_factor: 1000 * 1000 * 1000 / perf_freq,
                    };

                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        print_property(&mut parser, &property, false);
                    }
                }
                "MSNT_SystemTrace/PerfInfo/CollectionStart" => {
                    let mut parser = Parser::create(&s);
                    let interval_raw: u32 = parser.parse("NewInterval");
                    let interval_nanos = interval_raw as u64 * 100;
                    let interval = SamplingInterval::from_nanos(interval_nanos);
                    println!("Sample rate {}ms", interval.as_secs_f64() * 1000.);
                    profile.set_interval(interval);
                    context_switch_handler = ContextSwitchHandler::new(interval_raw as u64);
                }
                "MSNT_SystemTrace/Thread/SetName" => {
                    let mut parser = Parser::create(&s);

                    let process_id: u32 = parser.parse("ProcessId");
                    if !process_targets.contains(&process_id) {
                        return;
                    }
                    let thread_id: u32 = parser.parse("ThreadId");
                    let thread_name: String = parser.parse("ThreadName");
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let thread_start_instant = profile_start_instant;
                            let handle = match global_thread {
                                Some(global_thread) => global_thread,
                                None => {
                                    let process = processes[&process_id].process_handle;
                                    profile.add_thread(process, thread_id, thread_start_instant, false)
                                }
                            };
                            let tb = e.insert(
                                ThreadState::new(handle, thread_id)
                            );
                            thread_index += 1;
                            tb
                         }
                    };
                    if Some(thread.handle) != global_thread {
                        profile.set_thread_name(thread.handle, &thread_name);
                    }
                    thread.merge_name = Some(thread_name);
                }
                "MSNT_SystemTrace/Thread/Start" |
                "MSNT_SystemTrace/Thread/DCStart" => {
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("TThreadId");
                    let process_id: u32 = parser.parse("ProcessId");
                    //assert_eq!(process_id,s.process_id());
                    //println!("thread_name pid: {} tid: {} name: {:?}", process_id, thread_id, thread_name);

                    if !process_targets.contains(&process_id) {
                        return;
                    }

                    let thread_start_instant = profile_start_instant;
                    let handle = match global_thread {
                        Some(global_thread) => global_thread,
                        None => {
                            let process = processes.get_mut(&process_id).unwrap();

                            let is_main = process.main_thread_handle.is_none();
                            let thread_handle = profile.add_thread(process.process_handle, thread_id, timestamp, is_main);
                            if is_main {
                                process.main_thread_handle = Some(thread_handle);
                            }
                            thread_handle
                        }
                    };
                    let thread = ThreadState::new(handle, thread_id);

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => {
                            // Clobber the existing thread. We don't rely on thread end events to remove threads
                            // because they can be dropped and there can be subsequent events that refer to an ended thread.
                            // eg.
                            // MSNT_SystemTrace/Thread/End MSNT_SystemTrace 2-0 14 7369515373
                            //     ProcessId: InTypeUInt32 = 4532
                            //     TThreadId: InTypeUInt32 = 17524
                            // MSNT_SystemTrace/Thread/ReadyThread MSNT_SystemTrace 50-0 5 7369515411
                            //     TThreadId: InTypeUInt32 = 1644
                            // MSNT_SystemTrace/StackWalk/Stack MSNT_SystemTrace 32-0 35 7369515425
                            //     EventTimeStamp: InTypeUInt64 = 7369515411
                            //     StackProcess: InTypeUInt32 = 4532
                            //     StackThread: InTypeUInt32 = 17524
                            // MSNT_SystemTrace/Thread/CSwitch MSNT_SystemTrace 36-0 12 7369515482
                            //     NewThreadId: InTypeUInt32 = 1644
                            //     OldThreadId: InTypeUInt32 = 0

                            let existing = e.into_mut();
                            *existing = thread;
                            existing
                        }
                        Entry::Vacant(e) => {
                            e.insert(thread)
                        }
                    };

                    let thread_name: Result<String, _> = parser.try_parse("ThreadName");

                    match thread_name {
                        Ok(thread_name) if !thread_name.is_empty() => {
                            if Some(thread.handle) != global_thread {
                                profile.set_thread_name(thread.handle, &thread_name);
                            }
                            thread.merge_name = Some(thread_name)
                        },
                        _ => {}
                    }
                }
                "MSNT_SystemTrace/Thread/End" |
                "MSNT_SystemTrace/Thread/DCEnd" => {
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("TThreadId");
                    let process_id: u32 = parser.parse("ProcessId");

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => {
                            profile.set_thread_end_time(e.get().handle, timestamp);
                        }
                        Entry::Vacant(e) => {
                        }
                    };

                }
                "MSNT_SystemTrace/Process/Start" |
                "MSNT_SystemTrace/Process/DCStart" => {
                    if let Some(process_target_name) = &process_target_name {
                        let timestamp = e.EventHeader.TimeStamp as u64;
                        let timestamp = timestamp_converter.convert_raw(timestamp);
                        let mut parser = Parser::create(&s);


                        let image_file_name: String = parser.parse("ImageFileName");
                        println!("process start {}", image_file_name);

                        let process_id: u32 = parser.parse("ProcessId");
                        if image_file_name.contains(process_target_name) {
                            process_targets.insert(process_id);
                            println!("tracing {}", process_id);
                            let process_handle = match global_process {
                                Some(global_process) => global_process,
                                None => profile.add_process(&image_file_name, process_id, timestamp),
                            };

                            processes.insert(process_id, ProcessState::new(process_handle));
                        }
                    }
                }
                "MSNT_SystemTrace/StackWalk/Stack" => {
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("StackThread");
                    let process_id: u32 = parser.parse("StackProcess");

                    let timestamp: u64 = parser.parse("EventTimeStamp");
                    if !process_targets.contains(&process_id) {
                        // eprintln!("not watching");
                        return;
                    }
                    
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let thread_start_instant = profile_start_instant;
                            let handle = match global_thread {
                                Some(global_thread) => global_thread,
                                None => {
                                    let process = processes[&process_id].process_handle;
                                    profile.add_thread(process, thread_id, thread_start_instant, false)
                                }
                            };
                            let tb = e.insert(
                                ThreadState::new(handle, thread_id)
                            );
                            thread_index += 1;
                            tb
                        }
                    };
                    // eprint!("{} {} {}", thread_id, e.EventHeader.TimeStamp, timestamp);

                    // Iterate over the stack addresses, starting with the instruction pointer
                    let mut stack: Vec<StackFrame> = Vec::with_capacity(parser.buffer.len() / 8);
                    let mut address_iter = parser.buffer.chunks_exact(8).map(|a| u64::from_ne_bytes(a.try_into().unwrap()));
                    let Some(first_frame_address) = address_iter.next() else { return };
                    let first_frame_stack_mode = stack_mode_for_address(first_frame_address, 8);
                    stack.push(StackFrame::InstructionPointer(first_frame_address, first_frame_stack_mode));
                    for frame_address in address_iter {
                        let stack_mode = stack_mode_for_address(first_frame_address, 8);
                        stack.push(StackFrame::ReturnAddress(frame_address, stack_mode));
                    }

                    if first_frame_stack_mode == StackMode::Kernel {
                        if let Some(pending_stack ) = thread.pending_stacks.iter_mut().rev().find(|s| s.timestamp == timestamp) {
                            if let Some(kernel_stack) = pending_stack.kernel_stack.as_mut() {
                                eprintln!("Multiple kernel stacks for timestamp {timestamp} on thread {thread_id}");
                                kernel_stack.extend(&stack);
                            } else {
                                pending_stack.kernel_stack = Some(stack);
                            }
                        }
                        return;
                    }

                    // We now know that we have a user stack. User stacks always come last. Consume
                    // the pending stack with matching timestamp.

                    let mut add_sample = |thread: &ThreadState, process: &mut ProcessState, timestamp: u64, cpu_delta: CpuDelta, weight: i32, stack: Vec<StackFrame>| {
                        let profile_timestamp = timestamp_converter.convert_raw(timestamp);
                        let stack_index = unresolved_stacks.convert(stack.into_iter().rev());
                        let extra_label_frame = if let Some(global_thread) = global_thread {
                            let thread_name = thread.merge_name.as_ref().map(|x| strip_thread_numbers(x).to_owned()).unwrap_or_else(|| format!("thread {}", thread.thread_id));
                            Some(FrameInfo {
                                frame: fxprof_processed_profile::Frame::Label(profile.intern_string(&thread_name)),
                                category_pair: user_category,
                                flags: FrameFlags::empty(),
                            })
                        } else { None };
                        process.unresolved_samples.add_sample(thread.handle, profile_timestamp, timestamp, stack_index, cpu_delta, weight, extra_label_frame);
                    };

                    // Use this user stack for all pending stacks from this thread.
                    while thread.pending_stacks.front().is_some_and(|s| s.timestamp <= timestamp) {
                        let PendingStack {
                            timestamp,
                            kernel_stack,
                            off_cpu_sample_group,
                            on_cpu_sample_cpu_delta,
                        } = thread.pending_stacks.pop_front().unwrap();
                        let process = processes.get_mut(&process_id).unwrap();

                        if let Some(off_cpu_sample_group) = off_cpu_sample_group {
                            let OffCpuSampleGroup { begin_timestamp, end_timestamp, sample_count } = off_cpu_sample_group;

                            let cpu_delta_raw = context_switch_handler.consume_cpu_delta(&mut thread.context_switch_data);
                            let cpu_delta = CpuDelta::from_nanos(cpu_delta_raw as u64 * timestamp_converter.raw_to_ns_factor);

                            // Add a sample at the beginning of the paused range.
                            // This "first sample" will carry any leftover accumulated running time ("cpu delta").
                            add_sample(thread, process, begin_timestamp, cpu_delta, 1, stack.clone());

                            if sample_count > 1 {
                                // Emit a "rest sample" with a CPU delta of zero covering the rest of the paused range.
                                let weight = i32::try_from(sample_count - 1).unwrap_or(0) * 1;
                                add_sample(thread, process, end_timestamp, CpuDelta::ZERO, weight, stack.clone());
                            }
                        }

                        if let Some(cpu_delta) = on_cpu_sample_cpu_delta {
                            if let Some(mut combined_stack) = kernel_stack {
                                combined_stack.extend_from_slice(&stack[..]);
                                add_sample(thread, process, timestamp, cpu_delta, 1, combined_stack);
                            } else {
                                add_sample(thread, process, timestamp, cpu_delta, 1, stack.clone());
                            }
                            stack_sample_count += 1;
                        }
                    }
                }
                "MSNT_SystemTrace/PerfInfo/SampleProf" => {
                    let mut parser = Parser::create(&s);

                    let thread_id: u32 = parser.parse("ThreadId");
                    //println!("sample {}", thread_id);
                    sample_count += 1;

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(), 
                        Entry::Vacant(_) => {
                            if include_idle {
                                if let Some(global_thread) = global_thread {
                                    let mut frames = Vec::new();
                                    let thread_name = match thread_id {
                                        0 => "Idle",
                                        _ => "Other"
                                    };
                                    let timestamp = e.EventHeader.TimeStamp as u64;
                                    let timestamp = timestamp_converter.convert_raw(timestamp);

                                    frames.push(FrameInfo {
                                        frame: fxprof_processed_profile::Frame::Label(profile.intern_string(&thread_name)),
                                        category_pair: user_category,
                                        flags: FrameFlags::empty()
                                    });
                                    profile.add_sample(global_thread, timestamp, frames.into_iter(), Duration::ZERO.into(), 1);
                                }
                            }
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };

                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let off_cpu_sample_group = context_switch_handler.handle_on_cpu_sample(timestamp, &mut thread.context_switch_data);
                    let delta = context_switch_handler.consume_cpu_delta(&mut thread.context_switch_data);
                    let cpu_delta = CpuDelta::from_nanos(delta as u64 * timestamp_converter.raw_to_ns_factor);
                    thread.pending_stacks.push_back(PendingStack { timestamp, kernel_stack: None, off_cpu_sample_group, on_cpu_sample_cpu_delta: Some(cpu_delta) });
                }
                "MSNT_SystemTrace/PageFault/DemandZeroFault" => {
                    if !demand_zero_faults { return }

                    let thread_id: u32 = s.thread_id();
                    //println!("sample {}", thread_id);
                    sample_count += 1;

                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(_) => {
                            if include_idle {
                                if let Some(global_thread) = global_thread {
                                    let mut frames = Vec::new();
                                    let thread_name = match thread_id {
                                        0 => "Idle",
                                        _ => "Other"
                                    };
                                    let timestamp = e.EventHeader.TimeStamp as u64;
                                    let timestamp = timestamp_converter.convert_raw(timestamp);

                                    frames.push(FrameInfo {
                                        frame: fxprof_processed_profile::Frame::Label(profile.intern_string(&thread_name)),
                                        category_pair: user_category,
                                        flags: FrameFlags::empty(),
                                    });

                                    profile.add_sample(global_thread, timestamp, frames.into_iter(), Duration::ZERO.into(), 1);
                                }
                            }
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    thread.pending_stacks.push_back(PendingStack { timestamp, kernel_stack: None, off_cpu_sample_group: None, on_cpu_sample_cpu_delta: Some(CpuDelta::from_millis(1.0)) });
                }
                "MSNT_SystemTrace/PageFault/VirtualFree" => {
                    if !process_targets.contains(&e.EventHeader.ProcessId) {
                        return;
                    }
                    let mut parser = Parser::create(&s);
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let thread_id = e.EventHeader.ThreadId;
                    let counter = match memory_usage.entry(e.EventHeader.ProcessId) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(entry) => {
                            entry.insert(MemoryUsage { counter: profile.add_counter(processes[&e.EventHeader.ProcessId].process_handle, "VirtualAlloc", "Memory", "Amount of VirtualAlloc allocated memory"), value: 0. })
                        }
                    };
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(_) => {
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    let timing =  MarkerTiming::Instant(timestamp);
                    let mut text = String::new();
                    let region_size: u64 = parser.parse("RegionSize");
                    counter.value -= region_size as f64;

                    //println!("{} VirtualFree({}) = {}", e.EventHeader.ProcessId, region_size, counter.value);
                    
                    profile.add_counter_sample(counter.counter, timestamp, -(region_size as f64), 1);
                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        //dbg!(&property);
                        write_property(&mut text, &mut parser, &property, false);
                        text += ", "
                    }

                    profile.add_marker(thread.handle, CategoryHandle::OTHER, "VirtualFree", TextMarker(text), timing)
                }
                "MSNT_SystemTrace/PageFault/VirtualAlloc" => {
                    if !process_targets.contains(&e.EventHeader.ProcessId) {
                        return;
                    }
                    let mut parser = Parser::create(&s);
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let thread_id = e.EventHeader.ThreadId;
                    let counter = match memory_usage.entry(e.EventHeader.ProcessId) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(entry) => {
                            entry.insert(MemoryUsage { counter: profile.add_counter(processes[&e.EventHeader.ProcessId].process_handle, "VirtualAlloc", "Memory", "Amount of VirtualAlloc allocated memory"), value: 0. })
                        }
                    };
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(_) => {
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    let timing =  MarkerTiming::Instant(timestamp);
                    let mut text = String::new();
                    let region_size: u64 = parser.parse("RegionSize");
                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        //dbg!(&property);
                        write_property(&mut text, &mut parser, &property, false);
                        text += ", "
                    }
                    counter.value += region_size as f64;
                    //println!("{}.{} VirtualAlloc({}) = {}",  e.EventHeader.ProcessId, thread_id, region_size, counter.value);
                    
                    profile.add_counter_sample(counter.counter, timestamp, region_size as f64, 1);
                    profile.add_marker(thread.handle, CategoryHandle::OTHER, "VirtualAlloc", TextMarker(text), timing)
                }
                "KernelTraceControl/ImageID/" => {

                    let process_id = s.process_id();
                    if !process_targets.contains(&process_id) && process_id != 0 {
                        return;
                    }
                    let mut parser = Parser::create(&s);

                    let image_base: u64 = parser.try_parse("ImageBase").unwrap();
                    let timestamp = parser.try_parse("TimeDateStamp").unwrap();
                    let image_size: u32 = parser.try_parse("ImageSize").unwrap();
                    let binary_path: String = parser.try_parse("OriginalFileName").unwrap();
                    let path = binary_path;
                    libs.insert(image_base, (path, image_size, timestamp));
                }
                "KernelTraceControl/ImageID/DbgID_RSDS" => {
                    let mut parser = Parser::create(&s);

                    let process_id = s.process_id();
                    if !process_targets.contains(&process_id) && process_id != 0 {
                        return;
                    }
                    let image_base: u64 = parser.try_parse("ImageBase").unwrap();

                    let guid: GUID = parser.try_parse("GuidSig").unwrap();
                    let age: u32 = parser.try_parse("Age").unwrap();
                    let debug_id = DebugId::from_parts(Uuid::from_fields(guid.data1, guid.data2, guid.data3, &guid.data4), age);
                    let pdb_path: String = parser.try_parse("PdbFileName").unwrap();
                    //let pdb_path = Path::new(&pdb_path);
                    let (ref path, image_size, timestamp) = libs[&image_base];
                    let code_id = Some(format!("{timestamp:08X}{image_size:x}"));
                    let name = Path::new(path).file_name().unwrap().to_str().unwrap().to_owned();
                    let debug_name = Path::new(&pdb_path).file_name().unwrap().to_str().unwrap().to_owned();
                    let info = LibraryInfo { 
                        name,
                        debug_name,
                        path: path.clone(), 
                        code_id,
                        symbol_table: None, 
                        debug_path: pdb_path,
                        debug_id, 
                        arch: Some("x86_64".into())
                    };
                    if process_id == 0 {
                        kernel_pending_libraries.insert(image_base, info);
                    } else {
                        let process = processes.get_mut(&process_id).unwrap();
                        process.pending_libraries.insert(image_base, info);
                    }

                }
                "MSNT_SystemTrace/Image/Load" | "MSNT_SystemTrace/Image/DCStart" => {
                    // KernelTraceControl/ImageID/ and KernelTraceControl/ImageID/DbgID_RSDS are synthesized from MSNT_SystemTrace/Image/Load
                    // but don't contain the full path of the binary. We go through a bit of a dance to store the information from those events
                    // in pending_libraries and deal with it here. We assume that the KernelTraceControl events come before the Image/Load event.

                    let mut parser = Parser::create(&s);
                    // the ProcessId field doesn't necessarily match s.process_id();
                    let process_id = parser.try_parse("ProcessId").unwrap();
                    if !process_targets.contains(&process_id) && process_id != 0 {
                        return;
                    }
                    let image_base: u64 = parser.try_parse("ImageBase").unwrap();
                    let image_size: u64 = parser.try_parse("ImageSize").unwrap();

                    let path: String = parser.try_parse("FileName").unwrap();
                    // The filename is a NT kernel path (https://chrisdenton.github.io/omnipath/NT.html) which isn't direclty usable from user space.
                    // perfview goes through a dance to convert it to a regular user space path
                    // https://github.com/microsoft/perfview/blob/4fb9ec6947cb4e68ac7cb5e80f50ae3757d0ede4/src/TraceEvent/Parsers/KernelTraceEventParser.cs#L3461
                    // We'll just concatenate \\?\GLOBALROOT\
                    let path = format!("\\\\?\\GLOBALROOT{}", path);

                    let info = if process_id == 0 {
                        kernel_pending_libraries.remove(&image_base)
                    } else {
                        let process = processes.get_mut(&process_id).unwrap();
                        process.pending_libraries.remove(&image_base)
                    };
                    // If the file doesn't exist on disk we won't have KernelTraceControl/ImageID events
                    // This happens for the ghost drivers mentioned here: https://devblogs.microsoft.com/oldnewthing/20160913-00/?p=94305
                    if let Some(mut info) = info {
                        info.path = path;
                        let lib_handle = profile.add_lib(info);
                        if process_id == 0 {
                            profile.add_kernel_lib_mapping(lib_handle, image_base, image_base + image_size as u64, 0);
                        } else {
                            let process = processes.get_mut(&process_id).unwrap();
                            process.regular_lib_mapping_ops.push(e.EventHeader.TimeStamp as u64, LibMappingOp::Add(LibMappingAdd {
                                start_avma: image_base,
                                end_avma: image_base + image_size as u64,
                                relative_address_at_start: 0,
                                info: LibMappingInfo::new_lib(lib_handle),
                            }));
                        }
                    }
                }
                "Microsoft-Windows-DxgKrnl/VSyncDPC/Info " => {
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);

                    #[derive(Debug, Clone)]
                    pub struct VSyncMarker;

                    impl ProfilerMarker for VSyncMarker {
                        const MARKER_TYPE_NAME: &'static str = "Vsync";

                        fn json_marker_data(&self) -> Value {
                            json!({
                                "type": Self::MARKER_TYPE_NAME,
                                "name": ""
                            })
                        }

                        fn schema() -> MarkerSchema {
                            MarkerSchema {
                                type_name: Self::MARKER_TYPE_NAME,
                                locations: vec![MarkerLocation::MarkerChart, MarkerLocation::MarkerTable, MarkerLocation::TimelineOverview],
                                chart_label: Some("{marker.data.name}"),
                                tooltip_label: None,
                                table_label: Some("{marker.name} - {marker.data.name}"),
                                fields: vec![MarkerSchemaField::Dynamic(MarkerDynamicField {
                                    key: "name",
                                    label: "Details",
                                    format: MarkerFieldFormat::String,
                                    searchable: false,
                                })],
                            }
                        }
                    }

                    let gpu_thread = gpu_thread.get_or_insert_with(|| {
                        let gpu = profile.add_process("GPU", 1, profile_start_instant);
                        profile.add_thread(gpu, 1, profile_start_instant, false)
                    });
                    profile.add_marker(*gpu_thread,
                        CategoryHandle::OTHER,
                        "Vsync",
                        VSyncMarker{},
                        MarkerTiming::Instant(timestamp)
                    );
                }
                "MSNT_SystemTrace/Thread/CSwitch" => {
                    let mut parser = Parser::create(&s);
                    let new_thread: u32 = parser.parse("NewThreadId");
                    let old_thread: u32 = parser.parse("OldThreadId");
                    let timestamp = e.EventHeader.TimeStamp as u64;
                    // println!("CSwitch {} -> {} @ {} on {}", old_thread, new_thread, e.EventHeader.TimeStamp, unsafe { e.BufferContext.Anonymous.ProcessorIndex });
                    if let Some(old_thread) = threads.get_mut(&old_thread) {
                        context_switch_handler.handle_switch_out(timestamp, &mut old_thread.context_switch_data);
                    };
                    if let Some(new_thread) = threads.get_mut(&new_thread) {
                        let off_cpu_sample_group = context_switch_handler.handle_switch_in(timestamp, &mut new_thread.context_switch_data);
                        if let Some(off_cpu_sample_group) = off_cpu_sample_group {
                            new_thread.pending_stacks.push_back(PendingStack { timestamp, kernel_stack: None, off_cpu_sample_group: Some(off_cpu_sample_group), on_cpu_sample_cpu_delta: None });
                        }
                    };

                }
                "MSNT_SystemTrace/Thread/ReadyThread" => {
                    // these events can give us the unblocking stack
                    let mut parser = Parser::create(&s);
                    let _thread_id: u32 = parser.parse("TThreadId");
                }
                "V8.js/MethodLoad/" |
                "Microsoft-JScript/MethodRuntime/MethodDCStart" |
                "Microsoft-JScript/MethodRuntime/MethodLoad" => {
                    let mut parser = Parser::create(&s);
                    let method_name: String = parser.parse("MethodName");
                    let method_start_address: Address = parser.parse("MethodStartAddress");
                    let method_size: u64 = parser.parse("MethodSize");
                    // let source_id: u64 = parser.parse("SourceID");
                    let process_id = s.process_id();
                    let process = match processes.get_mut(&process_id) {
                        Some(process) => process,
                        None => {
                            // This event is probably from a process which doesn't match our name filter.
                            // Ignore it.
                            return;
                        }
                    };
                    let process_jit_info = jscript_symbols.entry(s.process_id()).or_insert_with(|| {
                        let lib_handle = profile.add_lib(LibraryInfo { name: format!("JIT-{process_id}"), debug_name: format!("JIT-{process_id}"), path: format!("JIT-{process_id}"), debug_path: format!("JIT-{process_id}"), debug_id: DebugId::nil(), code_id: None, arch: None, symbol_table: None });
                        ProcessJitInfo { lib_handle, jit_mapping_ops: LibMappingOpQueue::default(), next_relative_address: 0, symbols: Vec::new() }
                    });
                    let start_address = method_start_address.as_u64();
                    let relative_address = process_jit_info.next_relative_address;
                    process_jit_info.next_relative_address += method_size as u32;

                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);

                    if let Some(main_thread) = process.main_thread_handle {
                        profile.add_marker(
                            main_thread,
                            CategoryHandle::OTHER,
                            "JitFunctionAdd",
                            JitFunctionAddMarker(method_name.to_owned()),
                            MarkerTiming::Instant(timestamp),
                        );
                    }
                    
                    let (category, js_frame) = jit_category_manager.classify_jit_symbol(&method_name, &mut profile);
                    let info = LibMappingInfo::new_jit_function(process_jit_info.lib_handle, category, js_frame);
                    process_jit_info.jit_mapping_ops.push(e.EventHeader.TimeStamp as u64, LibMappingOp::Add(LibMappingAdd {
                        start_avma: start_address,
                        end_avma: start_address + method_size,
                        relative_address_at_start: relative_address,
                        info
                    }));
                    process_jit_info.symbols.push(Symbol {
                        address: relative_address,
                        size: Some(method_size as u32),
                        name: method_name,
                    });
                }
                "V8.js/SourceLoad/" /*|
                "Microsoft-JScript/MethodRuntime/MethodDCStart" |
                "Microsoft-JScript/MethodRuntime/MethodLoad"*/ => {
                    let mut parser = Parser::create(&s);
                    let source_id: u64 = parser.parse("SourceID");
                    let url: String = parser.parse("Url");
                    //if s.process_id() == 6736 { dbg!(s.process_id(), &method_name, method_start_address, method_size); }
                    jscript_sources.insert(source_id, url);
                    //dbg!(s.process_id(), jscript_symbols.keys());

                }
                "Microsoft-Windows-Direct3D11/ID3D11VideoContext_SubmitDecoderBuffers/win:Start" => {
                    let mut parser = Parser::create(&s);

                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let thread_id = e.EventHeader.ThreadId;
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(), 
                        Entry::Vacant(_) => {
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    let mut text = String::new();
                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        //dbg!(&property);
                        write_property(&mut text, &mut parser, &property, false);
                        text += ", "
                    }
                    thread.pending_markers.insert(s.name().to_owned(), PendingMarker { text, start: timestamp });
                }
                "Microsoft-Windows-Direct3D11/ID3D11VideoContext_SubmitDecoderBuffers/win:Stop" => {
                    let mut parser = Parser::create(&s);

                    let timestamp = e.EventHeader.TimeStamp as u64;
                    let timestamp = timestamp_converter.convert_raw(timestamp);
                    let thread_id = e.EventHeader.ThreadId;
                    let thread = match threads.entry(thread_id) {
                        Entry::Occupied(e) => e.into_mut(), 
                        Entry::Vacant(_) => {
                            dropped_sample_count += 1;
                            // We don't know what process this will before so just drop it for now
                            return;
                        }
                    };
                    
                    let mut text = String::new();
                    let timing = if let Some(pending) = thread.pending_markers.remove("Microsoft-Windows-Direct3D11/ID3D11VideoContext_SubmitDecoderBuffers/win:Start") {
                        text = pending.text;
                        MarkerTiming::Interval(pending.start, timestamp)
                    } else {
                        MarkerTiming::IntervalEnd(timestamp)
                    };

                    for i in 0..s.property_count() {
                        let property = s.property(i);
                        //dbg!(&property);
                        write_property(&mut text, &mut parser, &property, false);
                        text += ", "
                    }

                    let category = match categories.entry(s.provider_name()) {
                        Entry::Occupied(e) => *e.get(),
                        Entry::Vacant(e) => {
                            let category = profile.add_category(e.key(), CategoryColor::Transparent);
                            *e.insert(category)
                        }
                    };

                    profile.add_marker(thread.handle, category, s.name().split_once("/").unwrap().1, TextMarker(text), timing);
                }
                _ => {
                    if let Some(marker_name) = s.name().strip_prefix("Mozilla.FirefoxTraceLogger/").and_then(|s| s.strip_suffix("/")) {
                        let thread_id = e.EventHeader.ThreadId;
                        let thread = match threads.entry(thread_id) {
                            Entry::Occupied(e) => e.into_mut(), 
                            Entry::Vacant(_) => {
                                dropped_sample_count += 1;
                                // We don't know what process this will before so just drop it for now
                                return;
                            }
                        };
                        let mut parser = Parser::create(&s);
                        let mut text = String::new();
                        for i in 0..s.property_count() {
                            let property = s.property(i);
                            match property.name.as_str() {
                                "MarkerName" | "StartTime" | "EndTime" | "Phase" | "InnerWindowId" | "CategoryPair" => { continue; }
                                _ => {}
                            }
                            write_property(&mut text, &mut parser, &property, false);
                            text += ", "
                        }

                        /// From https://searchfox.org/mozilla-central/rev/0e7394a77cdbe1df5e04a1d4171d6da67b57fa17/mozglue/baseprofiler/public/BaseProfilerMarkersPrerequisites.h#355-360
                        const PHASE_INSTANT: u8 = 0;
                        const PHASE_INTERVAL: u8 = 1;
                        const PHASE_INTERVAL_START: u8 = 2;
                        const PHASE_INTERVAL_END: u8 = 3;

                        // We ignore e.EventHeader.TimeStamp and instead take the timestamp from the fields.
                        let start_time_qpc: u64 = parser.try_parse("StartTime").unwrap();
                        let end_time_qpc: u64 = parser.try_parse("EndTime").unwrap();
                        assert!(event_timestamps_are_qpc, "Inconsistent timestamp formats! ETW traces with Firefox events should be captured with QPC timestamps (-ClockType PerfCounter) so that ETW sample timestamps are compatible with the QPC timestamps in Firefox ETW trace events, so that the markers appear in the right place.");
                        let (phase, instant_time_qpc): (u8, u64) = match parser.try_parse("Phase") {
                            Ok(phase) => (phase, start_time_qpc),
                            Err(_) => {
                                // Before the landing of https://bugzilla.mozilla.org/show_bug.cgi?id=1882640 ,
                                // Firefox ETW trace events didn't have phase information, so we need to
                                // guess a phase based on the timestamps.
                                if start_time_qpc != 0 && end_time_qpc != 0 {
                                    (PHASE_INTERVAL, 0)
                                } else if start_time_qpc != 0 {
                                    (PHASE_INSTANT, start_time_qpc)
                                } else {
                                    (PHASE_INSTANT, end_time_qpc)
                                }
                            }
                        };
                        let timing = match phase {
                            PHASE_INSTANT => MarkerTiming::Instant(timestamp_converter.convert_raw(instant_time_qpc)),
                            PHASE_INTERVAL => MarkerTiming::Interval(timestamp_converter.convert_raw(start_time_qpc), timestamp_converter.convert_raw(end_time_qpc)),
                            PHASE_INTERVAL_START => MarkerTiming::IntervalStart(timestamp_converter.convert_raw(start_time_qpc)),
                            PHASE_INTERVAL_END => MarkerTiming::IntervalEnd(timestamp_converter.convert_raw(end_time_qpc)),
                            _ => panic!("Unexpected marker phase {phase}"),
                        };

                        if marker_name == "UserTiming" {
                            let name: String = parser.try_parse("name").unwrap();
                            profile.add_marker(thread.handle, CategoryHandle::OTHER, "UserTiming", UserTimingMarker(name), timing);
                        } else if marker_name == "SimpleMarker" || marker_name == "Text" || marker_name == "tracing" {
                            let marker_name: String = parser.try_parse("MarkerName").unwrap();
                            profile.add_marker(thread.handle, CategoryHandle::OTHER, &marker_name, TextMarker(text.clone()), timing);
                        } else {
                            profile.add_marker(thread.handle, CategoryHandle::OTHER, marker_name, TextMarker(text.clone()), timing);
                        }
                    } else if let Some(marker_name) = s.name().strip_prefix("Google.Chrome/").and_then(|s| s.strip_suffix("/")) {
                        // a bitfield of keywords
                        bitflags! {
                            #[derive(PartialEq, Eq)]
                            pub struct KeywordNames: u64 {
                                const benchmark = 0x1;
                                const blink = 0x2;
                                const browser = 0x4;
                                const cc = 0x8;
                                const evdev = 0x10;
                                const gpu = 0x20;
                                const input = 0x40;
                                const netlog = 0x80;
                                const sequence_manager = 0x100;
                                const toplevel = 0x200;
                                const v8 = 0x400;
                                const disabled_by_default_cc_debug = 0x800;
                                const disabled_by_default_cc_debug_picture = 0x1000;
                                const disabled_by_default_toplevel_flow = 0x2000;
                                const startup = 0x4000;
                                const latency = 0x8000;
                                const blink_user_timing = 0x10000;
                                const media = 0x20000;
                                const loading = 0x40000;
                                const base = 0x80000;
                                const devtools_timeline = 0x100000;
                                const unused_bit_21 = 0x200000;
                                const unused_bit_22 = 0x400000;
                                const unused_bit_23 = 0x800000;
                                const unused_bit_24 = 0x1000000;
                                const unused_bit_25 = 0x2000000;
                                const unused_bit_26 = 0x4000000;
                                const unused_bit_27 = 0x8000000;
                                const unused_bit_28 = 0x10000000;
                                const unused_bit_29 = 0x20000000;
                                const unused_bit_30 = 0x40000000;
                                const unused_bit_31 = 0x80000000;
                                const unused_bit_32 = 0x100000000;
                                const unused_bit_33 = 0x200000000;
                                const unused_bit_34 = 0x400000000;
                                const unused_bit_35 = 0x800000000;
                                const unused_bit_36 = 0x1000000000;
                                const unused_bit_37 = 0x2000000000;
                                const unused_bit_38 = 0x4000000000;
                                const unused_bit_39 = 0x8000000000;
                                const unused_bit_40 = 0x10000000000;
                                const unused_bit_41 = 0x20000000000;
                                const navigation = 0x40000000000;
                                const ServiceWorker = 0x80000000000;
                                const edge_webview = 0x100000000000;
                                const diagnostic_event = 0x200000000000;
                                const __OTHER_EVENTS = 0x400000000000;
                                const __DISABLED_OTHER_EVENTS = 0x800000000000;
                            }
                        }

                        let mut parser = Parser::create(&s);
                        let thread_id = e.EventHeader.ThreadId;
                        let phase: String = parser.try_parse("Phase").unwrap();

                        let thread = match threads.entry(thread_id) {
                            Entry::Occupied(e) => e.into_mut(), 
                            Entry::Vacant(_) => {
                                dropped_sample_count += 1;
                                // We don't know what process this will before so just drop it for now
                                return;
                            }
                        };
                        let mut text = String::new();
                        for i in 0..s.property_count() {
                            let property = s.property(i);
                            if property.name == "Timestamp" || property.name == "Phase" || property.name == "Duration" {
                                continue;
                            }
                            //dbg!(&property);
                            write_property(&mut text, &mut parser, &property, false);
                            text += ", "
                        }

                        // We ignore e.EventHeader.TimeStamp and instead take the timestamp from the fields.
                        let timestamp_us: u64 = parser.try_parse("Timestamp").unwrap();
                        let timestamp = timestamp_converter.convert_us(timestamp_us);

                        let timing = match phase.as_str() {
                            "Begin" => MarkerTiming::IntervalStart(timestamp),
                            "End" => MarkerTiming::IntervalEnd(timestamp),
                            _ => MarkerTiming::Instant(timestamp),
                        };
                        let keyword = KeywordNames::from_bits(e.EventHeader.EventDescriptor.Keyword).unwrap();
                        if keyword == KeywordNames::blink_user_timing {
                            profile.add_marker(thread.handle, CategoryHandle::OTHER, "UserTiming", UserTimingMarker(marker_name.to_owned()), timing);
                        } else {
                            profile.add_marker(thread.handle, CategoryHandle::OTHER, marker_name, TextMarker(text.clone()), timing);
                        }
                    } else {
                        let mut parser = Parser::create(&s);

                        let timestamp = e.EventHeader.TimeStamp as u64;
                        let timestamp = timestamp_converter.convert_raw(timestamp);
                        let thread_id = e.EventHeader.ThreadId;
                        let thread = match threads.entry(thread_id) {
                            Entry::Occupied(e) => e.into_mut(), 
                            Entry::Vacant(_) => {
                                dropped_sample_count += 1;
                                // We don't know what process this will before so just drop it for now
                                return;
                            }
                        };
                        let mut text = String::new();
                        for i in 0..s.property_count() {
                            let property = s.property(i);
                            //dbg!(&property);
                            write_property(&mut text, &mut parser, &property, false);
                            text += ", "
                        }

                        let timing = MarkerTiming::Instant(timestamp);
                        let category = match categories.entry(s.provider_name()) {
                            Entry::Occupied(e) => *e.get(),
                            Entry::Vacant(e) => {
                                let category = profile.add_category(e.key(), CategoryColor::Transparent);
                                *e.insert(category)
                            }
                        };

                        profile.add_marker(thread.handle, category, s.name().split_once("/").unwrap().1, TextMarker(text), timing)
                    }
                     //println!("unhandled {}", s.name()) 
                    }
            }
            //println!("{}", name);
        }
    });

    if !result.is_ok() {
        dbg!(&result);
        std::process::exit(1);
    }

    let (marker_spans, sample_ranges) = match marker_file {
        Some(marker_file) => get_markers(
            &marker_file,
            marker_prefix.as_deref(),
            timestamp_converter,
        )
        .expect("Could not get markers"),
        None => (Vec::new(), None),
    };

    // Push queued samples into the profile.
    // We queue them so that we can get symbolicated JIT function names. To get symbolicated JIT function names,
    // we have to call profile.add_sample after we call profile.set_lib_symbol_table, and we don't have the
    // complete JIT symbol table before we've seen all JIT symbols.
    // (This is a rather weak justification. The better justification is that this is consistent with what
    // samply does on Linux and macOS, where the queued samples also want to respect JIT function names from
    // a /tmp/perf-1234.map file, and this file may not exist until the profiled process finishes.)
    let mut stack_frame_scratch_buf = Vec::new();
    for (process_id, process) in processes {
        let ProcessState { unresolved_samples, regular_lib_mapping_ops, main_thread_handle, .. } = process;
        let jitdump_lib_mapping_op_queues = match jscript_symbols.remove(&process_id) {
            Some(jit_info) => {
                profile.set_lib_symbol_table(jit_info.lib_handle, Arc::new(SymbolTable::new(jit_info.symbols)));
                vec![jit_info.jit_mapping_ops]
            },
            None => Vec::new(),
        };
        let process_sample_data = ProcessSampleData::new(unresolved_samples, regular_lib_mapping_ops, jitdump_lib_mapping_op_queues, None, main_thread_handle.unwrap_or_else(|| panic!("process no main thread {:?}", process_id)));
        process_sample_data.flush_samples_to_profile(&mut profile, user_category, kernel_category, &mut stack_frame_scratch_buf, &mut unresolved_stacks, &[], &marker_spans, sample_ranges.as_ref())
    }

    /*if merge_threads {
        profile.add_thread(global_thread);
    } else {
        for (_, thread) in threads.drain() { profile.add_thread(thread.builder); }
    }*/

    let f = File::create("gecko.json").unwrap();
    to_writer(BufWriter::new(f), &profile).unwrap();
    println!("Took {} seconds", (Instant::now()-start).as_secs_f32());
    println!("{} events, {} samples, {} dropped, {} stack-samples", event_count, sample_count, dropped_sample_count, stack_sample_count);
}
