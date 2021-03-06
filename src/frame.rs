use gfx_hal::Backend;
use gfx_hal::window::{Backbuffer, Frame};

/// This wrapper allows the `Graph` to abstract over two different `FrameBuffer` scenarios.
///
/// ### Type parameters:
///
/// - `B`: hal `Backend`
#[derive(Derivative)]
#[derivative(Clone, Copy, Debug)]
pub enum SuperFrame<'a, B: Backend> {
    /// Index to one of multiple `Framebuffer`s created by the graph.
    Index(usize),
    /// Single `Framebuffer` associated with the `Swapchain`.
    Buffer(&'a B::Framebuffer),
}

impl<'a, B> SuperFrame<'a, B>
where
    B: Backend,
{
    /// Create a new `SuperFrame` from `Backbuffer` and `Frame` index.
    pub fn new(backbuffer: &'a Backbuffer<B>, frame: Frame) -> Self {
        // Check if we have `Framebuffer` from `Surface` (usually with OpenGL backend) or `Image`s
        // In case it's `Images` we need to pick `Framebuffer` for `RenderPass`es
        // that renders onto surface.
        match *backbuffer {
            Backbuffer::Images(_) => SuperFrame::Index(frame.id()),
            Backbuffer::Framebuffer(ref single) => SuperFrame::Buffer(single),
        }
    }

    /// Get index of the frame
    pub fn index(&self) -> usize {
        match *self {
            SuperFrame::Index(index) => index,
            SuperFrame::Buffer(_) => 0,
        }
    }
}

/// Framebuffer wrapper
#[derive(Debug)]
pub enum SuperFramebuffer<B: Backend> {
    /// Target is multiple `Framebuffer`s created over `ImageView`s
    Owned(Vec<B::Framebuffer>),

    /// Target is single `Framebuffer` associated with `Swapchain`
    External,
}

/// Pick the correct framebuffer
pub fn pick<'a, B>(
    framebuffer: &'a SuperFramebuffer<B>,
    frame: &SuperFrame<'a, B>,
) -> &'a B::Framebuffer
where
    B: Backend,
{
    use self::SuperFrame::*;
    use self::SuperFramebuffer::*;
    match (framebuffer, frame) {
        (&Owned(ref framebuffers), ref frame) => &framebuffers[frame.index()],
        (&External, &Buffer(ref framebuffer)) => framebuffer,
        _ => unreachable!("This combination can't happen"),
    }
}
