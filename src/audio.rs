use std::cmp::Ordering;
use std::fmt::{Debug, Display, Formatter, Write};
use std::io;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{
    default_host, BuildStreamError, Device, FromSample, SizedSample, Stream, StreamConfig,
    StreamError
};
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use songbird::input::reader::MediaSource;
use songbird::input::{Input, Reader};

pub struct InputDeviceListItem {
    pub device: Device,
    pub name: String,
    pub is_default: bool
}

impl Debug for InputDeviceListItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        struct Device;

        impl Debug for Device {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                write!(f, "<Device>")
            }
        }

        f.debug_struct("InputDeviceListItem")
            .field("device", &Device)
            .field("name", &self.name)
            .field("is_default", &self.is_default)
            .finish()
    }
}

impl Display for InputDeviceListItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

pub fn list_input_devices() -> anyhow::Result<Vec<InputDeviceListItem>> {
    let host = default_host();
    let input_devices = host.input_devices()?;
    let Some(default_input_device) = host.default_input_device()
    else {
        return Ok(Vec::new());
    };
    let default_input_device_name = default_input_device.name()?;

    let size_hint = input_devices.size_hint();
    let mut devices = Vec::with_capacity(size_hint.1.unwrap_or(size_hint.0));
    for input_device in input_devices {
        let name = input_device.name()?;
        let is_default = name == default_input_device_name;
        devices.push(InputDeviceListItem {
            device: input_device,
            name,
            is_default
        });
    }

    Ok(devices)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct SampleRate(u32);

impl SampleRate {
    pub fn new(value: u32) -> Self {
        Self(value)
    }
}

impl From<SampleRate> for cpal::SampleRate {
    fn from(value: SampleRate) -> Self {
        cpal::SampleRate(value.0)
    }
}

impl Display for SampleRate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.0 % 1000 {
            0 => write!(f, "{:>2} kHz", self.0 / 1000),
            _ => write!(f, "{} Hz", self.0)
        }
    }
}

impl PartialEq<cpal::SampleRate> for SampleRate {
    fn eq(&self, other: &cpal::SampleRate) -> bool {
        self.0.eq(&other.0)
    }
}

impl PartialOrd<cpal::SampleRate> for SampleRate {
    fn partial_cmp(&self, other: &cpal::SampleRate) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

pub struct CpalMediaSource {
    data: Arc<ArrayQueue<u8>>,
    error: Arc<AtomicCell<Option<StreamError>>>,
    is_stereo: bool
}

impl CpalMediaSource {
    pub fn from_device<T>(
        device: &Device,
        stream_config: &StreamConfig
    ) -> Result<(Self, Stream), BuildStreamError>
    where
        T: SizedSample,
        f32: FromSample<T>
    {
        let is_stereo = match stream_config.channels {
            1 => false,
            2 => true,
            channels => return Err(BuildStreamError::StreamConfigNotSupported)
        };

        let data_consumer = Arc::new(ArrayQueue::new(1024));
        let data_producer = data_consumer.clone();
        let error = Arc::new(AtomicCell::new(None));
        let callback_error = error.clone();

        let input_stream = device.build_input_stream(
            stream_config,
            move |data: &[T], _| {
                for date in data.iter() {
                    // songbird needs data to be encoded directly as opus, raw i16 or raw f32
                    let sample: f32 = date.to_sample();
                    for byte in sample.to_ne_bytes() {
                        // TODO: if necessary, handle this more elegantly
                        let _ = data_producer.push(byte);
                    }
                }
            },
            move |stream_error| callback_error.store(Some(stream_error)),
            None
        )?;

        Ok((
            CpalMediaSource {
                data: data_consumer,
                error,
                is_stereo
            },
            input_stream
        ))
    }

    pub fn into_input(self) -> Input {
        Input::float_pcm(self.is_stereo, Reader::Extension(Box::new(self)))
    }
}

impl Read for CpalMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.error.swap(None) {
            Some(StreamError::DeviceNotAvailable) => {
                return Err(io::Error::new(
                    ErrorKind::BrokenPipe,
                    "source got disconnected"
                ))
            }
            Some(err) => return Err(io::Error::new(ErrorKind::Other, err.to_string())),
            None => ()
        }

        for (i, byte_ref) in buf.iter_mut().enumerate() {
            match (self.data.pop(), i) {
                (None, 0) => return Err(io::ErrorKind::WouldBlock.into()),
                (None, _) => return Ok(i),
                (Some(byte), _) => *byte_ref = byte
            }
        }

        Ok(buf.len())
    }
}

impl Seek for CpalMediaSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        // The source does not provide seekability but the trait `MediaSource` requires
        // this
        unimplemented!()
    }
}

impl MediaSource for CpalMediaSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}