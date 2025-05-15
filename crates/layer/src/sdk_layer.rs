//! The main tracing layer and related utils exposed by this crate.
use std::sync::atomic::{self, AtomicU64};
use std::{env, process, sync, thread};

use tracing::span;
use tracing_perfetto_sdk_sys::ffi::{self, get_tracing_init_count};
use tracing_subscriber::{layer, registry};

use crate::ids::thread_id;
use crate::{debug_annotations, ids, init};

/// A layer to be used with `tracing-subscriber` that forwards collected spans
/// to the Perfetto SDK via the C++ API.
#[derive(Clone)]
#[repr(transparent)]
pub struct SdkLayer {
    inner: sync::Arc<Inner>,
}

struct ThreadLocalCtx {
    descriptor_sent: AtomicU64,
}

struct Inner {
    process_track_uuid: ids::TrackUuid,
    process_descriptor_sent: atomic::AtomicU64,
    thread_local_ctxs: thread_local::ThreadLocal<ThreadLocalCtx>,
}

impl SdkLayer {
    /// Create a new instance of [`SdkLayer`].
    pub fn new(name: &str) -> Self {
        // Shared global initialization for all layers
        init::global_init(
            name,
            false,
            true,
        );

        let process_track_uuid = ids::TrackUuid::for_process(process::id());
        let process_descriptor_sent = atomic::AtomicU64::new(0);
        let thread_local_ctxs = thread_local::ThreadLocal::new();

        let inner = sync::Arc::new(Inner {
            process_track_uuid,
            process_descriptor_sent,
            thread_local_ctxs,
        });
        Self { inner }
    }
    

    fn ensure_context_known(&self) {
        let current_count = get_tracing_init_count() + 1;
        self.ensure_process_known(current_count);
        self.ensure_thread_known(current_count);
    }

    fn ensure_process_known(&self, count: u64) {
        let process_descriptor_sent = self
            .inner
            .process_descriptor_sent
            .load(atomic::Ordering::Relaxed);
        if process_descriptor_sent == count {
            return;
        }
        if self
            .inner
            .process_descriptor_sent
            .compare_exchange(
                process_descriptor_sent,
                count,
                atomic::Ordering::Relaxed,
                atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            let process_name = env::current_exe()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let process_name = process_name.rsplit("/").next().unwrap_or("");

            ffi::trace_track_descriptor_process(
                0,
                self.inner.process_track_uuid.as_raw(),
                process_name,
                process::id(),
            );
        }
    }

    fn ensure_thread_known(&self, count: u64) {
        let thread_local_ctx = self
            .inner
            .thread_local_ctxs
            .get_or(|| ThreadLocalCtx { descriptor_sent: AtomicU64::new(0) });
        if thread_local_ctx.descriptor_sent.load(atomic::Ordering::Relaxed) == count {
            return;
        }
        thread_local_ctx.descriptor_sent.store(count, atomic::Ordering::Relaxed);
        let tid = thread_id();
        ffi::trace_track_descriptor_thread(
            self.inner.process_track_uuid.as_raw(),
            ids::TrackUuid::for_thread(tid).as_raw(),
            process::id(),
            thread::current().name().unwrap_or(""),
            thread_id() as u32,
        );
    }
}

impl<S> tracing_subscriber::Layer<S> for SdkLayer
where
    S: tracing::Subscriber,
    S: for<'a> registry::LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
        attrs.record(&mut debug_annotations);

        span.extensions_mut().insert(debug_annotations);
    }

    fn on_record(&self, id: &tracing::Id, values: &span::Record<'_>, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let mut extensions = span.extensions_mut();
        if let Some(debug_annotations) =
            extensions.get_mut::<debug_annotations::FFIDebugAnnotations>()
        {
            values.record(debug_annotations);
        } else {
            let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
            values.record(&mut debug_annotations);
            extensions.insert(debug_annotations);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: layer::Context<'_, S>) {
        self.ensure_context_known();

        let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
        event.record(&mut debug_annotations);

        if !debug_annotations.suppress_event() {
            let meta = event.metadata();
            let track_uuid = ids::TrackUuid::for_thread(thread_id());
            ffi::trace_track_event_instant(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
                &debug_annotations.as_ffi(),
            );
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let track_uuid = ids::TrackUuid::for_thread(thread_id());
        let meta = span.metadata();

        self.ensure_context_known();
        ffi::trace_track_event_slice_begin(
            track_uuid.as_raw(),
            meta.name(),
            meta.file().unwrap_or_default(),
            meta.line().unwrap_or_default(),
            &span
                .extensions()
                .get()
                .unwrap_or(&debug_annotations::FFIDebugAnnotations::default())
                .as_ffi(),
        );
    }

    fn on_exit(&self, id: &tracing::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let track_uuid = ids::TrackUuid::for_thread(thread_id());

        ffi::trace_track_event_slice_end(
            track_uuid.as_raw(),
            meta.name(),
            meta.file().unwrap_or_default(),
            meta.line().unwrap_or_default(),
        );
    }
}
