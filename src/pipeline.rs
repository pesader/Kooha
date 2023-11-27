use std::os::fd::RawFd;

use anyhow::{ensure, Context, Result};
use gst::prelude::*;
use gst_pbutils::prelude::*;
use gtk::{
    gdk,
    glib::{self, clone, closure_local},
    subclass::prelude::*,
};

use crate::{
    audio_device::{self, Class as AudioDeviceClass},
    screencast_session::Stream,
    utils,
};

const PREVIEW_FRAME_RATE: i32 = 30;

const COMPOSITOR_NAME: &str = "compositor";
const TEE_NAME: &str = "tee";
const PAINTABLE_SINK_NAME: &str = "paintablesink";
const DESKTOP_AUDIO_LEVEL_NAME: &str = "desktop-audio-level";
const MICROPHONE_LEVEL_NAME: &str = "microphone-level";

#[derive(Debug, Clone, Copy, glib::Boxed)]
#[boxed_type(name = "KoohaStreamSize", nullable)]
pub struct StreamSize {
    width: i32,
    height: i32,
}

impl StreamSize {
    pub fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }

    pub fn width(self) -> i32 {
        self.width
    }

    pub fn height(self) -> i32 {
        self.height
    }
}

#[derive(Debug, Clone, Copy, glib::Boxed)]
#[boxed_type(name = "KoohaPeaks")]
pub struct Peaks {
    left: f64,
    right: f64,
}

impl Peaks {
    pub fn new(left: f64, right: f64) -> Self {
        Self { left, right }
    }

    pub fn left(&self) -> f64 {
        self.left
    }

    pub fn right(&self) -> f64 {
        self.right
    }
}

mod imp {
    use std::cell::{Cell, RefCell};

    use glib::{once_cell::sync::Lazy, subclass::Signal};
    use gst::bus::BusWatchGuard;

    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::Pipeline)]
    pub struct Pipeline {
        #[property(get)]
        pub(super) stream_size: Cell<Option<StreamSize>>,

        pub(super) inner: gst::Pipeline,
        pub(super) bus_watch_guard: RefCell<Option<BusWatchGuard>>,

        pub(super) recording_elements: RefCell<Vec<gst::Element>>,
        pub(super) video_elements: RefCell<Vec<gst::Element>>,

        pub(super) desktop_audio_elements: RefCell<Vec<gst::Element>>,
        pub(super) microphone_elements: RefCell<Vec<gst::Element>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Pipeline {
        const NAME: &'static str = "KoohaPipeline";
        type Type = super::Pipeline;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Pipeline {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            if let Err(err) = obj.setup() {
                tracing::error!("Failed to setup pipeline: {:?}", err);
            }
        }

        fn dispose(&self) {
            if let Err(err) = self.inner.set_state(gst::State::Null) {
                tracing::error!("Failed to set state to Null {:?}", err);
            }

            let _ = self.bus_watch_guard.take();
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: Lazy<Vec<Signal>> = Lazy::new(|| {
                vec![
                    Signal::builder("desktop-audio-peak")
                        .param_types([Peaks::static_type()])
                        .build(),
                    Signal::builder("microphone-peak")
                        .param_types([Peaks::static_type()])
                        .build(),
                ]
            });

            SIGNALS.as_ref()
        }
    }
}

glib::wrapper! {
    pub struct Pipeline(ObjectSubclass<imp::Pipeline>);
}

impl Pipeline {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn connect_desktop_audio_peak<F>(&self, f: F) -> glib::SignalHandlerId
    where
        F: Fn(&Self, &Peaks) + 'static,
    {
        self.connect_closure(
            "desktop-audio-peak",
            true,
            closure_local!(|obj: &Self, peaks: &Peaks| {
                f(obj, peaks);
            }),
        )
    }

    pub fn connect_microphone_peak<F>(&self, f: F) -> glib::SignalHandlerId
    where
        F: Fn(&Self, &Peaks) + 'static,
    {
        self.connect_closure(
            "microphone-peak",
            true,
            closure_local!(|obj: &Self, peaks: &Peaks| {
                f(obj, peaks);
            }),
        )
    }

    pub fn paintable(&self) -> gdk::Paintable {
        self.imp()
            .inner
            .by_name(PAINTABLE_SINK_NAME)
            .unwrap()
            .property("paintable")
    }

    pub fn start_recording(&self) -> Result<()> {
        let imp = self.imp();

        ensure!(
            imp.recording_elements.borrow().is_empty(),
            "Already recording"
        );

        let tee = imp.inner.by_name(TEE_NAME).unwrap();

        let video_profile =
            gst_pbutils::EncodingVideoProfile::builder(&gst::Caps::builder("video/x-vp8").build())
                .preset("Profile Realtime")
                .variable_framerate(true)
                .build();
        let audio_profile = gst_pbutils::EncodingAudioProfile::builder(
            &gst::Caps::builder("audio/x-vorbis").build(),
        )
        .build();
        let profile = gst_pbutils::EncodingContainerProfile::builder(
            &gst::Caps::builder("video/webm").build(),
        )
        .name("WebM audio/video")
        .description("Standard WebM/VP8/Vorbis")
        .add_profile(video_profile)
        .add_profile(audio_profile)
        .build();

        let encodebin = gst::ElementFactory::make("encodebin")
            .property("profile", profile)
            .build()?;
        let filesink = gst::ElementFactory::make("filesink")
            .property("location", "/var/home/dave/test.webm")
            .build()?;

        let elements = vec![encodebin.clone(), filesink.clone()];
        imp.inner.add_many(&elements)?;

        tee.link(&encodebin)?;
        encodebin.link(&filesink)?;

        for element in &elements {
            element.sync_state_with_parent()?;
        }

        imp.recording_elements.replace(elements);

        tracing::debug!("Started recording");

        Ok(())
    }

    pub fn stop_recording(&self) -> Result<()> {
        let imp = self.imp();

        let recording_elements = imp.recording_elements.take();

        ensure!(!recording_elements.is_empty(), "Not recording");

        for element in recording_elements {
            element.set_state(gst::State::Null)?;
            imp.inner.remove(&element)?;
        }

        tracing::debug!("Stopped recording");

        Ok(())
    }

    pub fn set_streams(&self, streams: &[Stream], fd: RawFd) -> Result<()> {
        let imp = self.imp();

        for element in imp.video_elements.take() {
            element.set_state(gst::State::Null)?;
            imp.inner.remove(&element)?;
        }

        let compositor = imp.inner.by_name(COMPOSITOR_NAME).unwrap();

        let videorate_caps = gst::Caps::builder("video/x-raw")
            .field("framerate", gst::Fraction::new(PREVIEW_FRAME_RATE, 1))
            .build();

        let mut last_pos = 0;
        for stream in streams {
            let pipewiresrc = gst::ElementFactory::make("pipewiresrc")
                .property("fd", fd)
                .property("path", stream.node_id().to_string())
                .property("do-timestamp", true)
                .property("keepalive-time", 1000)
                .property("resend-last", true)
                .build()?;
            let videorate = gst::ElementFactory::make("videorate").build()?;
            let videorate_capsfilter = gst::ElementFactory::make("capsfilter")
                .property("caps", &videorate_caps)
                .build()?;

            let elements = [pipewiresrc, videorate, videorate_capsfilter.clone()];
            imp.inner.add_many(&elements)?;
            gst::Element::link_many(&elements)?;
            imp.video_elements.borrow_mut().extend(elements);

            let compositor_sink_pad = compositor
                .request_pad_simple("sink_%u")
                .context("Failed to request sink_%u pad from compositor")?;
            compositor_sink_pad.set_property("xpos", last_pos);
            videorate_capsfilter
                .static_pad("src")
                .unwrap()
                .link(&compositor_sink_pad)?;

            let (stream_width, _) = stream.size().context("stream is missing size")?;
            last_pos += stream_width;
        }

        for element in imp.video_elements.borrow().iter() {
            element.sync_state_with_parent()?;
        }

        imp.stream_size.set(None);
        self.notify_stream_size();

        tracing::debug!("Loaded {} streams", streams.len());

        imp.inner.set_state(gst::State::Playing)?;

        Ok(())
    }

    pub async fn load_desktop_audio(&self) -> Result<()> {
        let imp = self.imp();

        if !imp.desktop_audio_elements.borrow().is_empty() {
            return Ok(());
        }

        let device_name = audio_device::find_default_name(AudioDeviceClass::Sink)
            .await
            .context("No desktop audio source found")?;

        let pulsesrc = gst::ElementFactory::make("pulsesrc")
            .property("device", &device_name)
            .build()?;
        let audioconvert = gst::ElementFactory::make("audioconvert").build()?;
        let level = gst::ElementFactory::make("level")
            .name(DESKTOP_AUDIO_LEVEL_NAME)
            .property("interval", gst::ClockTime::from_mseconds(80))
            .property("peak-ttl", gst::ClockTime::from_mseconds(80))
            .build()?;
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()?;

        let elements = vec![pulsesrc, audioconvert, level, fakesink];
        imp.inner.add_many(&elements)?;
        gst::Element::link_many(&elements)?;

        for element in &elements {
            element.sync_state_with_parent()?;
        }

        imp.desktop_audio_elements.replace(elements);

        tracing::debug!("Loaded desktop audio from {}", device_name);

        Ok(())
    }

    pub fn unload_desktop_audio(&self) -> Result<()> {
        let imp = self.imp();

        for element in imp.desktop_audio_elements.take() {
            element.set_state(gst::State::Null)?;
            imp.inner.remove(&element)?;
        }

        tracing::debug!("Unloaded desktop audio");

        Ok(())
    }

    pub async fn load_microphone(&self) -> Result<()> {
        let imp = self.imp();

        if !imp.microphone_elements.borrow().is_empty() {
            return Ok(());
        }

        let device_name = audio_device::find_default_name(AudioDeviceClass::Source)
            .await
            .context("No desktop audio source found")?;

        let pulsesrc = gst::ElementFactory::make("pulsesrc")
            .property("device", &device_name)
            .build()?;
        let audioconvert = gst::ElementFactory::make("audioconvert").build()?;
        let level = gst::ElementFactory::make("level")
            .name(MICROPHONE_LEVEL_NAME)
            .property("interval", gst::ClockTime::from_mseconds(80))
            .property("peak-ttl", gst::ClockTime::from_mseconds(80))
            .build()?;
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()?;

        let elements = vec![pulsesrc, audioconvert, level, fakesink];
        imp.inner.add_many(&elements)?;
        gst::Element::link_many(&elements)?;

        for element in &elements {
            element.sync_state_with_parent()?;
        }

        imp.microphone_elements.replace(elements);

        tracing::debug!("Loaded microphone from {}", device_name);

        Ok(())
    }

    pub fn unload_microphone(&self) -> Result<()> {
        let imp = self.imp();

        for element in imp.microphone_elements.take() {
            element.set_state(gst::State::Null)?;
            imp.inner.remove(&element)?;
        }

        tracing::debug!("Unloaded microphone");

        Ok(())
    }

    fn handle_bus_message(&self, message: &gst::Message) -> glib::ControlFlow {
        let imp = self.imp();

        match message.view() {
            gst::MessageView::AsyncDone(_) => {
                if imp.stream_size.get().is_some() {
                    return glib::ControlFlow::Continue;
                }

                let compositor = imp.inner.by_name(COMPOSITOR_NAME).unwrap();
                let caps = compositor
                    .static_pad("src")
                    .unwrap()
                    .current_caps()
                    .unwrap();
                let caps_struct = caps.structure(0).unwrap();
                let stream_width = caps_struct.get::<i32>("width").unwrap();
                let stream_height = caps_struct.get::<i32>("height").unwrap();

                imp.stream_size
                    .set(Some(StreamSize::new(stream_width, stream_height)));
                self.notify_stream_size();

                glib::ControlFlow::Continue
            }
            gst::MessageView::Element(e) => {
                if let Some(src) = e.src() {
                    if let Some(structure) = e.structure() {
                        if structure.has_name("level") {
                            let peaks = structure.get::<&glib::ValueArray>("rms").unwrap();
                            let left_peak = peaks.nth(0).unwrap().get::<f64>().unwrap();
                            let right_peak = peaks.nth(1).unwrap().get::<f64>().unwrap();

                            let normalized_left_peak = 10_f64.powf(left_peak / 20.0);
                            let normalized_right_peak = 10_f64.powf(right_peak / 20.0);

                            match src.name().as_str() {
                                DESKTOP_AUDIO_LEVEL_NAME => {
                                    self.emit_by_name::<()>(
                                        "desktop-audio-peak",
                                        &[&Peaks::new(normalized_left_peak, normalized_right_peak)],
                                    );
                                }
                                MICROPHONE_LEVEL_NAME => {
                                    self.emit_by_name::<()>(
                                        "microphone-peak",
                                        &[&Peaks::new(normalized_left_peak, normalized_right_peak)],
                                    );
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }

                glib::ControlFlow::Continue
            }
            gst::MessageView::Error(e) => {
                tracing::error!(src = ?e.src(), error = ?e.error(), debug = ?e.debug(), "Error from video bus");

                glib::ControlFlow::Break
            }
            _ => {
                tracing::trace!(?message, "Message from video bus");

                glib::ControlFlow::Continue
            }
        }
    }

    fn setup(&self) -> Result<()> {
        let imp = self.imp();

        let compositor = gst::ElementFactory::make("compositor")
            .name(COMPOSITOR_NAME)
            .build()?;
        let convert = gst::ElementFactory::make("videoconvert")
            .property("chroma-mode", gst_video::VideoChromaMode::None)
            .property("dither", gst_video::VideoDitherMethod::None)
            .property("matrix-mode", gst_video::VideoMatrixMode::OutputOnly)
            .property("n-threads", utils::ideal_thread_count())
            .build()?;
        let tee = gst::ElementFactory::make("tee").name(TEE_NAME).build()?;
        let sink = gst::ElementFactory::make("gtk4paintablesink")
            .name(PAINTABLE_SINK_NAME)
            .build()?;

        imp.inner.add_many([&compositor, &convert, &tee, &sink])?;
        gst::Element::link_many([&compositor, &convert, &tee])?;

        let tee_src_pad = tee
            .request_pad_simple("src_%u")
            .context("Failed to request sink_%u pad from compositor")?;
        tee_src_pad.link(&sink.static_pad("sink").unwrap())?;

        let bus_watch_guard = imp.inner.bus().unwrap().add_watch_local(
            clone!(@weak self as obj => @default-panic, move |_, message| {
                obj.handle_bus_message(message)
            }),
        )?;

        imp.bus_watch_guard.replace(Some(bus_watch_guard));

        Ok(())
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}
