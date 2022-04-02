use std::{
    collections::{HashMap, HashSet},
    error::Error,
    io::{stdin, stdout, Write},
    sync::{Arc, Mutex},
    time::Duration,
    thread
};
use rppal::gpio::{Gpio, Level};
use midir::{Ignore, MidiInput};
use rodio::Source;
use std::f32::consts::PI;
use rodio::{OutputStream, Sink};
const SAMPLE_RATE: usize = 44_000;

#[allow(unused)]
#[derive(Debug, Clone, Copy)]
enum WaveType {
    Sine,
    Square,
    Saw,
    Triangle,
}

#[derive(Clone, Debug)]
struct Wave {
    freq: f32,
    num_sample: usize,
    typ: WaveType,
    state: f32,
}

impl Wave {
    fn new(freq: f32, typ: WaveType) -> Wave {
        Wave {
            freq,
            typ,
            num_sample: 0,
            state: 0.0,
        }
    }
}

impl Iterator for Wave {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.num_sample = self.num_sample.wrapping_add(1);
        let period = 1.0 / self.freq * SAMPLE_RATE as f32;

        Some(match self.typ {
            WaveType::Sine => {
                (2.0 * PI * self.freq * self.num_sample as f32 / (SAMPLE_RATE as f32)).sin()
            }
            WaveType::Saw => {
                self.state = 2.0
                    * ((self.num_sample as f32 / period)
                        - (0.5 + (self.num_sample as f32 / period)).floor());
                self.state
            }
            WaveType::Square => {
                if self.num_sample % (period as usize) <= (period / 2.0) as usize {
                    1f32
                } else {
                    -1f32
                }
            }
            WaveType::Triangle => {
                self.state = 2.0
                    * (2.0
                        * ((self.num_sample as f32 / period)
                            - (self.num_sample as f32 / period + 0.5).floor()))
                    .abs()
                    - 1.0;
                self.state
            }
        })
    }
}

impl Source for Wave {
    #[inline]
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    #[inline]
    fn channels(&self) -> u16 {
        1
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE as u32
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct Adsr {
    attack: usize,
    decay: usize,
    sustain: f32,
    release: usize,
}

#[derive(Clone, Debug)]
struct Voice {
    freq: Arc<Mutex<f32>>,
    wave_type: WaveType,
    amp_env: Adsr,
    sink_idx: usize,
    releasing: Arc<Mutex<bool>>,
}

const INIT_SINK: Option<Sink> = None;
const MAX_POLYPHONY: usize = 16;
static mut SINKS: [Option<Sink>; MAX_POLYPHONY] = [INIT_SINK; MAX_POLYPHONY];

// Safe wrapper to get an immutable reference to a sink
fn get_sink(sink_idx: usize) -> &'static Sink {
    unsafe { SINKS[sink_idx].as_ref().unwrap() }
}

impl Voice {
    fn new(freq: f32, wave_type: WaveType, amp_env: Adsr, sink_idx: usize) -> Self {
        Self {
            freq: Arc::new(Mutex::new(freq)),
            wave_type,
            amp_env,
            sink_idx,
            releasing: Arc::new(Mutex::new(false)),
        }
    }

    fn play(&self) {
        let wave = Wave::new(*self.freq.lock().unwrap(), self.wave_type);

        let sink = get_sink(self.sink_idx);

        let attack = self.amp_env.attack;
        let decay = self.amp_env.decay;
        let sustain = self.amp_env.sustain;
        let release = self.amp_env.release;

        let mut volume = 0.0f32;
        let mut num_sample_released = 0usize;

        const SAMPLE_RATE_MS: usize = SAMPLE_RATE / 1000;

        let attack_num_samples = attack * SAMPLE_RATE_MS;
        let decay_num_samples = decay * SAMPLE_RATE_MS;
        let release_num_samples = release * SAMPLE_RATE_MS;

        let attack_step = 1.0 / attack_num_samples as f32;
        let decay_step = (1.0 - sustain) / decay_num_samples as f32;
        let release_step = sustain / release_num_samples as f32;

        let freq = self.freq.clone();
        let releasing = self.releasing.clone();
        sink.append(
            wave.amplify(volume)
                .stoppable()
                .periodic_access(Duration::from_millis(1), move |src| {
                    if *releasing.lock().unwrap() && num_sample_released == 0 {
                        num_sample_released = src.inner().inner().num_sample;
                        dbg!(num_sample_released);
                    } else if *releasing.lock().unwrap() {
                        let num_sample = src.inner().inner().num_sample - num_sample_released;
                        if num_sample < release_num_samples {
                            volume -= release_step;
                        } else {
                            src.stop();
                            dbg!("stopping!");
                        }
                    } else if src.inner().inner().num_sample < attack_num_samples {
                        volume += attack_step;
                    } else if (src.inner().inner().num_sample - attack_num_samples)
                        < decay_num_samples
                    {
                        volume -= decay_step;
                    }

                    src.inner_mut().set_factor(volume)
                })
                .periodic_access(Duration::from_nanos(50), move |src| {
                    // reset the frequency (used for pitch bend)
                    let target_freq = *freq.lock().unwrap();
                    let current_freq = &mut src.inner_mut().inner_mut().inner_mut().freq;
                    if *current_freq != target_freq {
                        if *current_freq > target_freq {
                            *current_freq -= 1.0;
                        } else {
                            *current_freq += 1.0;
                        }
                    }
                }),
        );

        sink.play();
    }

    fn stop(&self) {
        let mut releasing_lock = self.releasing.lock().unwrap();
        *releasing_lock = true;
    }
}

static PINS: [u8; 12] = [5, 6, 13, 19, 26, 21, 20, 16, 12, 25, 23, 24];

fn main() {
    for pin in PINS {
        let listener = EventListener::new_rising(pin, move || {
            println!("Triggered pog! {}",pin);
        });
    }
        match run() {
            Ok(_) => (),
            Err(err) => println!("Error: {}", err),
        }
}

fn midi_note_to_freq(midi_note: u8) -> f32 {
    2f32.powf((midi_note as f32 - 69.0) / 12.0) * 440.0
}

fn run() -> Result<(), Box<dyn Error>> {
    let (_stream, stream_handle) = OutputStream::try_default().unwrap();
    for i in 0..MAX_POLYPHONY {
        unsafe {
            SINKS[i] = Some(Sink::try_new(&stream_handle).unwrap());
        }
    }

    let mut playing_notes = HashMap::<u8, Voice>::new();
    let mut sustained_notes = HashSet::<u8>::new();

    let mut input = String::new();

    let mut midi_in = MidiInput::new("midir reading input")?;
    midi_in.ignore(Ignore::None);

    // Get an input port (read from console if multiple are available)
    let in_ports = midi_in.ports();
    let in_port = match in_ports.len() {
        0 => return Err("no input port found".into()),
        1 => {
            println!(
                "Choosing the only available input port: {}",
                midi_in.port_name(&in_ports[0]).unwrap()
            );
            &in_ports[0]
        }
        _ => {
            println!("\nAvailable input ports:");
            for (i, p) in in_ports.iter().enumerate() {
                println!("{}: {}", i, midi_in.port_name(p).unwrap());
            }
            print!("Please select input port: ");
            stdout().flush()?;
            let mut input = String::new();
            stdin().read_line(&mut input)?;
            in_ports
                .get(input.trim().parse::<usize>()?)
                .ok_or("invalid input port selected")?
        }
    };

    println!("\nOpening connection");
    let in_port_name = midi_in.port_name(in_port)?;

    // _conn_in needs to be a named parameter, because it needs to be kept alive until the end of the scope
    let _conn_in = midi_in.connect(
        in_port,
        "midir-read-input",
        move |stamp, message, _| {
            let status = message[0];
            let data1 = message[1];

            match status {
                // note on
                144..=159 => {
                    if let Some(existing_voice) = playing_notes.get(&data1) {
                        existing_voice.play();
                    } else {
                        let sink_idx = {
                            let mut found_idx = None;
                            for i in 0..MAX_POLYPHONY {
                                if get_sink(i).empty() {
                                    found_idx = Some(i);
                                    break;
                                }
                            }

                            if found_idx.is_none() {
                                for i in 0..MAX_POLYPHONY {
                                    if get_sink(i).is_paused() {
                                        get_sink(i).stop();
                                        unsafe {
                                            SINKS[i] = Some(Sink::try_new(&stream_handle).unwrap())
                                        };
                                        found_idx = Some(i);
                                        break;
                                    }
                                }
                            }

                            found_idx
                        };
                        if let Some(sink_idx) = sink_idx {
                            let freq = midi_note_to_freq(data1);
                            let note = Voice::new(
                                freq,
                                WaveType::Triangle,
                                Adsr {
                                    attack: 5,
                                    decay: 0,
                                    sustain: 0.8,
                                    release: 500,
                                },
                                // Adsr {
                                //     attack: 200,
                                //     decay: 400,
                                //     sustain: 0.4,
                                //     release: 1000,
                                // },
                                sink_idx,
                            );
                            note.play();
                            playing_notes.insert(data1, note);
                        } else {
                            dbg!("max polyphony hit");
                        }
                    }
                }
                // note off
                128..=143 => {
                    let note = playing_notes.get(&data1);
                    if let Some(note) = note {
                        if !sustained_notes.contains(&data1) {
                            note.stop();
                            playing_notes.remove(&data1);
                        }
                    }
                }
                // mode change
                183 => {
                    println!("{}: {:?} (len = {})", stamp, message, message.len());
                    // sus
                    if data1 == 64 {
                        let data2 = message[2];
                        match data2 {
                            127 => {
                                for (note_midi, note) in &playing_notes {
                                    if !get_sink(note.sink_idx).is_paused() {
                                        sustained_notes.insert(*note_midi);
                                    }
                                }
                            }
                            0 => {
                                for note_midi in &sustained_notes {
                                    let note = playing_notes.get(note_midi).unwrap();
                                    note.stop();
                                }

                                sustained_notes.clear();
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                // pitch bend
                231 => {
                    let bend_factor = message[2]; // 0-127 (64 means no bend)
                    for (midi_note, playing_voice) in playing_notes.iter_mut() {
                        *playing_voice.freq.lock().unwrap() =
                            midi_note_to_freq(*midi_note) + (bend_factor as f32 - 64.0);
                    }
                }
                _ => {
                    println!("{}: {:?} (len = {})", stamp, message, message.len());
                }
            }
        },
        (),
    )?;

    println!(
        "Connection open, reading input from '{}' (press enter to exit) ...",
        in_port_name
    );

    input.clear();
    stdin().read_line(&mut input)?; // wait for next enter key press

    println!("Closing connection");
    Ok(())
}

struct EventListener {
    pin: u8,
    handle: thread::JoinHandle<()>,
    stop: Arc<Mutex<bool>>,
}

impl EventListener {
    fn new_rising<Callback>(pin: u8, callback: Callback) -> Self
    where Callback: Fn() + std::marker::Send + 'static {
        let stop = Arc::new(Mutex::new(false));
        let stop_for_inner = stop.clone();
        let handle = thread::spawn(move || {
            let mut pin = Gpio::new().unwrap().get(pin).unwrap().into_input_pulldown();
            
            let mut prev_value = Level::High;
            while !*stop_for_inner.lock().unwrap() {
                let value = pin.read();
                if value == Level::High && prev_value == Level::Low {
                    callback();
                    prev_value = Level::High;
                } else if value == Level::Low && prev_value == Level::High {
                    prev_value = Level::Low;
                }
            }
        });
        Self {
            pin,
            handle,
            stop,
        }
    }

    fn stop(&self) {
        *self.stop.lock().unwrap() = true;
    }

    fn wait(self) {
        self.handle.join().unwrap();
    }
}


