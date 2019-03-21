#[allow(unused_imports)]
#[macro_use]
extern crate text_io;

use std::collections::VecDeque;
use std::sync::mpsc;

use arrayfire as af;
#[allow(unused_imports)]
use gnuplot;
use itertools::Itertools;
#[allow(unused_imports)]
use rayon::prelude::*;

use native::*;
use utils::save_plots;
use operations::Data;

pub mod ddm;
pub mod native;
pub mod operations;
#[macro_use]
pub mod utils;

type RawType = f32;
type RawFtType = num_complex::Complex32;

#[allow(unused_macros)]
macro_rules! print_wait {
    ($item:expr) => {
        af::print_gen("".to_string(), &$item, Some(2));
        let _: u32 = read!("{}");
    };
}

macro_rules! fft_shift {
    ($item:expr) => {
        //TODO: Why do I have to shift by a third?
        arrayfire::shift(
            &$item,
            &[
                ($item.dims()[0] / 2) as i32,
                ($item.dims()[1] / 3) as i32,
                1,
                1,
            ],
        );
    };
}

fn get_closest_power(x: i64) -> i64 {
    let xf64 = x as f64;
    let power2 = f64::log2(xf64).ceil() as i64;
    let power3 = f64::log(xf64, 3.0f64).ceil() as i64;
    let power5 = f64::log(xf64, 5.0f64).ceil() as i64;
    let minima = (0..=power2)
        .cartesian_product((0..=power3).cartesian_product(0..=power5))
        .map(|(a, (b, c))| {
            (2.0f64.powf(a as f64) * 3.0f64.powf(b as f64) * 5.0f64.powf(c as f64)) as i64
        })
        .filter(|&value| value >= x)
        .min();
    match minima {
        Some(n) => n,
        None => panic!("No suitable dimension!"),
    }
}

fn set_backend() {
    let backends = af::get_available_backends();
    let cuda_available = backends.iter().filter(|&x| *x == af::Backend::CUDA).count();
    let opencl_available = backends
        .iter()
        .filter(|&x| *x == af::Backend::OPENCL)
        .count();
    if cuda_available == 1 {
        af::set_backend(af::Backend::CUDA);
    } else if opencl_available == 1 {
        af::set_backend(af::Backend::OPENCL);
    } else {
        af::set_backend(af::Backend::CPU);
    }
}

fn process_arguments(args: Vec<String>) -> (Option<usize>, Option<String>) {
    let args_slice = args.as_slice();
    match args_slice {
        [_, command, path] if command == "video" => (
            Some(opencv::start_capture_safe(path)),
            match std::path::Path::new(path).file_stem() {
                Some(s) => Some(String::from(s.to_str().unwrap())),
                None => None
            }
        ),
        [_, command] if command == "camera" => (Some(opencv::start_camera_capture_safe()), None),
        _ => (None, None),
    }
}

enum Signal {
    KILL,
}

fn main() {
    set_backend();
    let (tx, rx) = mpsc::channel::<Option<af::Array<RawFtType>>>();
    let (stx, srx) = mpsc::channel::<Signal>();
    let (annuli_tx, annuli_rx) = mpsc::channel::<Vec<(f32, arrayfire::Array<crate::RawType>)>>();

    // Args processing should extract!
    let (id, filename) = process_arguments(std::env::args().collect::<Vec<String>>());

    let mut odim: Option<i64> = None;
    let annuli_spacing = 20;

    if let Some(id) = id {
        let output_dir = if let Some(v) = filename {
            v
        } else {
            String::from("camera")
        };
        println!(
            "Analysis of {} stream started!",
            &output_dir
        );
        let fps = opencv::fps(id);
        let frame_count = opencv::frame_count(id);
        println!(
            "Video is about {} seconds long, containing {} frames!",
            (frame_count as f64) / (fps as f64),
            frame_count
        );
        let mut counter = 1u32;
        let stream_thread = std::thread::spawn(move || loop {
            let frame = opencv::GrayImage::get_frame(id);
            match frame {
                None => match tx.send(None) {
                    _ => {
                        break;
                    }
                },
                Some(value) => {
                    if odim == None {
                        let n = std::cmp::max(value.cols, value.rows);
                        odim = Some(get_closest_power(n as i64));
                        match annuli_tx.send(operations::generate_annuli(odim, annuli_spacing)) {
                            Ok(_) => println!("Generated annuli!"),
                            Err(e) => {
                                panic!("Failed to generate annuli - {}!", e);
                            }
                        }
                    }
                    let ft = fft_shift!(af::fft2(&value.data, 1.0, odim.unwrap(), odim.unwrap()));
                    match tx.send(Some(ft)) {
                        Ok(_) => {
                            println!("ft {} - complete!", counter);
                        }
                        Err(_) => {
                            println!("Failed to send frame!");
                        }
                    }
                    counter += 1;
                }
            }
            if let Ok(Signal::KILL) = srx.try_recv() {
                break;
            }
        });

        let capacity = fps; //* 1;

        let mut counter_t0 = 0;
        let mut data: Data<crate::RawFtType> = Data::new(fps, Some(capacity));
        let mut collected_all_frames = false;

        let mut acc: Option<VecDeque<af::Array<RawType>>> = None;
        loop {
            match rx.recv() {
                Ok(value) => {
                    if let Some(v) = value {
                        data.push(v);
                    }
                }
                Err(e) => match std::sync::mpsc::TryRecvError::from(e) {
                    std::sync::mpsc::TryRecvError::Disconnected => {
                        collected_all_frames = true;
                    }
                    std::sync::mpsc::TryRecvError::Empty => {
                        continue;
                    }
                },
            }

            if data.data.len() == capacity {
                if let Some(a) = acc {
                    acc = Some(ddm::ddm(a, &data.data));
                } else {
                    acc = Some(ddm::ddm_0(&data.data));
                };
                counter_t0 += 1;
                println!("Analysis of t0 = {} done!", counter_t0);
            }

            if collected_all_frames {
                if let Some(a) = acc {
                    let acc = a
                        .par_iter()
                        .map(|x| x / (counter_t0 as f32))
                        .collect::<Vec<af::Array<RawType>>>();
                    let annuli = match annuli_rx.recv() {
                        Ok(v) => v,
                        Err(e) => {
                            panic!("Failed to receive annuli - {}!", e);
                        }
                    };
                    let a = annuli.iter().map(|(_, a)| {a.clone()}).collect::<Vec<af::Array<RawType>>>();
                    let radial_averaged = operations::radial_average(&acc, &annuli);
                    let radial_average_transposed =
                        operations::transpose_2d_array(&radial_averaged);
                    //TODO: I vs q for various tau
                    //create plots here
                    save_plots(&output_dir, radial_averaged);
                    save_plots(&(format!("{}_vs_tau", &output_dir)), radial_average_transposed);
                }
                break;
            }
        }
        println!(
            "Analysis of {} stream complete!",
            &output_dir
        );
        match stx.send(Signal::KILL) {
            Ok(_) | Err(_) => {
                stream_thread.join().unwrap();
                opencv::close_stream_safe(id);
                utils::print_times();
            }
        };
    } else {
        println!("No arguments supplied!");
    }
}
