
use std::borrow::Borrow;
use std::collections::HashMap;
use std::ops::Range;
use std::ptr::eq;

use failure::{Error, ResultExt, err_msg};
use gfx_hal::{Backend, Device};
use gfx_hal::device::Extent;
use gfx_hal::format::{AspectFlags, Format, Swizzle};
use gfx_hal::image::{Kind, AaMode, Level, SubresourceRange, Usage};
use gfx_hal::memory::Properties;
use gfx_hal::pso::PipelineStage;
use gfx_hal::window::Backbuffer;

use attachment::{Attachment, AttachmentImageViews, ColorAttachment, ColorAttachmentDesc, DepthStencilAttachment, DepthStencilAttachmentDesc, InputAttachmentDesc};
use graph::Graph;
use pass::{PassBuilder, PassNode};

pub const COLOR_RANGE: SubresourceRange = SubresourceRange {
    aspects: AspectFlags::COLOR,
    levels: 0..1,
    layers: 0..1,
};

pub struct GraphBuilder<'a, B: Backend, T> {
    passes: Vec<PassBuilder<'a, B, T>>,
    present: Option<&'a ColorAttachment>,
    backbuffer: Option<&'a Backbuffer<B>>,
    extent: Extent,
}

impl<'a, B, T> GraphBuilder<'a, B, T>
where
    B: Backend,
{
    pub fn new() -> Self {
        GraphBuilder {
            passes: Vec::new(),
            present: None,
            backbuffer: None,
            extent: Extent {
                width: 0,
                height: 0,
                depth: 0,
            },
        }
    }

    pub fn with_pass(mut self, pass: PassBuilder<'a, B, T>) -> Self {
        self.add_pass(pass);
        self
    }

    pub fn add_pass(&mut self, pass: PassBuilder<'a, B, T>) {
        self.passes.push(pass);
    }

    pub fn with_extent(mut self, extent: Extent) -> Self {
        self.set_extent(extent);
        self
    }

    pub fn set_extent(&mut self, extent: Extent) {
        self.extent = extent;
    }

    pub fn with_backbuffer(mut self, backbuffer: &'a Backbuffer<B>) -> Self {
        self.set_backbuffer(backbuffer);
        self
    }

    pub fn set_backbuffer(&mut self, backbuffer: &'a Backbuffer<B>) {
        self.backbuffer = Some(backbuffer);
    }

    pub fn with_present(mut self, present: &'a ColorAttachment) -> Self {
        self.set_present(present);
        self
    }

    pub fn set_present(&mut self, present: &'a ColorAttachment) {
        self.present = Some(present);
    }

    /// Build rendering graph from `ColorPin`
    /// for specified `backbuffer`.
    pub fn build<A, I>(
        self,
        device: &B::Device,
        mut allocator: A,
    ) -> Result<Graph<B, I, T>, Error>
    where
        A: FnMut(
        Kind,
        Level,
        Format,
        Usage,
        Properties,
        &B::Device) -> Result<I, Error>,
        I: Borrow<B::Image>,
    {
        let present = self.present.ok_or(err_msg("Failed to build Graph. Present attachment has to be set"))?;
        let backbuffer = self.backbuffer.ok_or(err_msg("Failed to build Graph. Backbuffer has to be set"))?;
        // Create views for backbuffer
        let (mut image_views, frames) = match *backbuffer {
            Backbuffer::Images(ref images) => (
                images
                    .iter()
                    .map(|image| {
                        device.create_image_view(
                            image,
                            present.format,
                            Swizzle::NO,
                            COLOR_RANGE.clone(),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(|err| format!("Failed to build graph: {}", err))?,
                images.len(),
            ),
            Backbuffer::Framebuffer(_) => (vec![], 1),
        };

        // Reorder passes to maximise overlapping
        // while keeping all dependencies before dependants.
        let (passes, deps) = reorder_passes(self.passes);

        let color_attachments = color_attachments(&passes);
        let depth_stencil_attachments = depth_stencil_attachments(&passes);

        // Setup image storage
        let mut images = vec![];

        // Initialize all targets
        let mut color_targets = HashMap::<*const _, (Range<usize>, usize)>::new();
        color_targets.insert(present, (0..image_views.len(), 0));
        for attachment in color_attachments {
            if eq(attachment, present) {
                color_targets.insert(
                    attachment,
                    (
                        create_target::<B, _, I>(
                            attachment.format,
                            &mut allocator,
                            device,
                            &mut images,
                            &mut image_views,
                            self.extent,
                            frames,
                        )?,
                        0,
                    ),
                );
            }
        }

        let mut depth_stencil_targets = HashMap::<*const _, (Range<usize>, usize)>::new();
        for attachment in depth_stencil_attachments {
            depth_stencil_targets.insert(
                attachment,
                (
                    create_target::<B, _, I>(
                        attachment.format,
                        &mut allocator,
                        device,
                        &mut images,
                        &mut image_views,
                        self.extent,
                        frames,
                    )?,
                    0,
                ),
            );
        }

        // Build pass nodes from pass builders
        let mut pass_nodes: Vec<PassNode<B, T>> = Vec::new();

        let mut first_draws_to_surface = None;

        for (pass, last_dep) in passes.into_iter().zip(deps) {
            // Collect input targets
            let inputs = pass.inputs
                .iter()
                .map(|input| {
                    let input = input.unwrap();
                    let (ref indices, ref written) = *match input {
                        Attachment::Color(color) => &color_targets[&color.ptr()],
                        Attachment::DepthStencil(depth_stencil) => {
                            &depth_stencil_targets[&depth_stencil.ptr()]
                        }
                    };
                    let indices = indices.clone();
                    debug_assert!(*written > 0);
                    let ref view = image_views[indices];
                    InputAttachmentDesc {
                        format: input.format(),
                        view,
                    }
                })
                .collect::<Vec<_>>();

            let colors = pass.colors
                .iter()
                .enumerate()
                .map(|(index, color)| {
                    let color = color.unwrap();
                    if first_draws_to_surface.is_none() && eq(color, present) {
                        first_draws_to_surface = Some(index);
                    }
                    let (ref indices, ref mut written) =
                        *color_targets.get_mut(&color.ptr()).unwrap();
                    let indices = indices.clone();
                    let clear = if *written == 0 { color.clear } else { None };

                    *written += 1;

                    ColorAttachmentDesc {
                        format: color.format,
                        view: if indices != (0..0) {
                            AttachmentImageViews::Owned(&image_views[indices])
                        } else {
                            AttachmentImageViews::External
                        },
                        clear,
                    }
                })
                .collect::<Vec<_>>();

            let depth_stencil = pass.depth_stencil.clone().map(|(depth, _stencil)| {
                let depth = depth.unwrap();
                let (ref indices, ref mut written) =
                    *depth_stencil_targets.get_mut(&depth.ptr()).unwrap();
                let indices = indices.clone();
                let clear = if *written == 0 { depth.clear } else { None };

                *written += 1;

                DepthStencilAttachmentDesc {
                    format: depth.format,
                    view: if indices != (0..0) {
                        AttachmentImageViews::Owned(&image_views[indices])
                    } else {
                        AttachmentImageViews::External
                    },
                    clear,
                }
            });

            let mut node = pass.build(device, &inputs[..], &colors[..], depth_stencil, self.extent)?;

            if let Some(last_dep) = last_dep {
                node.depends = if pass_nodes
                    .iter()
                    .find(|node| {
                        node.depends
                            .as_ref()
                            .map(|&(id, _)| id == last_dep)
                            .unwrap_or(false)
                    })
                    .is_none()
                {
                    // No passes prior this depends on `last_dep`
                    Some((last_dep, PipelineStage::TOP_OF_PIPE)) // Pick better
                } else {
                    None
                };
            }

            pass_nodes.push(node);
        }

        let mut signals = Vec::new();
        for i in 0..pass_nodes.len() {
            if let Some(j) = pass_nodes.iter().position(|node| {
                node.depends
                    .as_ref()
                    .map(|&(id, _)| id == i)
                    .unwrap_or(false)
            }) {
                // j depends on i
                assert!(
                    pass_nodes
                        .iter()
                        .skip(j + 1)
                        .find(|node| node.depends
                            .as_ref()
                            .map(|&(id, _)| id == i)
                            .unwrap_or(false))
                        .is_none()
                );
                signals.push(Some(device.create_semaphore()));
            } else {
                signals.push(None);
            }
        }

        Ok(Graph {
            passes: pass_nodes,
            signals,
            images,
            views: image_views,
            frames,
            first_draws_to_surface: first_draws_to_surface.unwrap(),
        })
    }
}


fn reorder_passes<'a, B, T: 'a>(
    mut unscheduled: Vec<PassBuilder<'a, B, T>>,
) -> (Vec<PassBuilder<'a, B, T>>, Vec<Option<usize>>)
where
    B: Backend,
{
    // Ordered passes
    let mut scheduled = vec![];
    let mut deps = vec![];

    // Until we schedule all unscheduled passes
    while !unscheduled.is_empty() {
        // Walk over unscheduled
        let (last_dep, index) = (0..unscheduled.len())
            .filter(|&index| {
                // Check if all dependencies are scheduled
                dependencies(&unscheduled, &unscheduled[index]).is_empty()
            }).map(|index| {
                // Find indices for all direct dependencies of the pass
                let dependencies = direct_dependencies(&scheduled, &unscheduled[index]);
                let siblings = siblings(&scheduled, &unscheduled[index]);
                (dependencies.into_iter().chain(siblings).max(), index)
            })
            // Smallest index of last dependency wins. `None < Some(0)`
            .min_by_key(|&(last_dep, _)| last_dep)
            // At least one pass with all dependencies scheduled must be found.
            // Or there is dependency circle in unscheduled left.
            .expect("Circular dependency encountered");

        // Store
        scheduled.push(unscheduled.swap_remove(index));
        deps.push(last_dep);
        unscheduled.swap_remove(index);
    }
    (scheduled, deps)
}

/// Get all color attachments for all passes
fn color_attachments<'a, B, T>(passes: &[PassBuilder<'a, B, T>]) -> Vec<&'a ColorAttachment>
where
    B: Backend,
{
    let mut attachments = Vec::new();
    for pass in passes {
        attachments.extend(pass.colors.iter().cloned().map(Option::unwrap));
    }
    attachments.sort_by_key(|a| a as *const _);
    attachments.dedup_by_key(|a| a as *const _);
    attachments
}

/// Get all depth_stencil attachments for all passes
fn depth_stencil_attachments<'a, B, T>(
    passes: &[PassBuilder<'a, B, T>],
) -> Vec<&'a DepthStencilAttachment>
where
    B: Backend,
{
    let mut attachments = Vec::new();
    for pass in passes {
        attachments.extend(pass.depth_stencil.as_ref().map(|&(a, _)| a.unwrap()));
    }
    attachments.sort_by_key(|a| a as *const _);
    attachments.dedup_by_key(|a| a as *const _);
    attachments
}

fn create_target<B, A, I>(
    format: Format,
    mut allocator: A,
    device: &B::Device,
    images: &mut Vec<I>,
    views: &mut Vec<B::ImageView>,
    extent: Extent,
    frames: usize,
) -> Result<Range<usize>, Error>
where
    B: Backend,
    A: FnMut(
        Kind,
        Level,
        Format,
        Usage,
        Properties,
        &B::Device) -> Result<I, Error>,
    I: Borrow<B::Image>,
{
    let kind = Kind::D2(
        extent.width as u16,
        extent.height as u16,
        AaMode::Single,
    );
    let start = views.len();
    for _ in 0..frames {
        let image = allocator(
            kind,
            1,
            format,
            Usage::COLOR_ATTACHMENT,
            Properties::DEVICE_LOCAL,
            device,
        )?;
        let view = device.create_image_view(image.borrow(), format, Swizzle::NO, COLOR_RANGE.clone())?;
        views.push(view);
        images.push(image);
    }
    Ok(start..views.len())
}

/// Get dependencies of pass.
fn direct_dependencies<'a, B, T>(
    passes: &'a [PassBuilder<'a, B, T>],
    pass: &'a PassBuilder<'a, B, T>,
) -> Vec<usize>
where
    B: Backend,
{
    let mut deps = Vec::new();
    for input in &pass.inputs {
        let input = input.unwrap();
        deps.extend(passes.iter().enumerate().filter(|p| {
            p.1.depth_stencil
                .as_ref()
                .map(|&(a, _)| input.is(Attachment::DepthStencil(a.unwrap())))
                .unwrap_or(false) || p.1.colors.iter().any(|a| input.is(Attachment::Color(a.unwrap())))
        }).map(|p| p.0));
    }
    deps.sort();
    deps.dedup();
    deps
}

/// Get other passes that shares output attachments
fn siblings<'a, B, T>(
    passes: &'a [PassBuilder<'a, B, T>],
    pass: &'a PassBuilder<'a, B, T>,
) -> Vec<usize>
where
    B: Backend,
{
    let mut siblings = Vec::new();
    for &color in pass.colors.iter() {
        siblings.extend(passes.iter().enumerate().filter(|p| {
            p.1.colors
                .iter()
                .any(|a| eq(a.unwrap(), color.unwrap()))
        }).map(|p| p.0));
    }
    if let Some((Some(depth), _)) = pass.depth_stencil {
        siblings.extend(passes.iter().enumerate().filter(|p| {
            p.1.depth_stencil
                .as_ref()
                .map(|&(a, _)| eq(a.unwrap(), depth))
                .unwrap_or(false)
        }).map(|p| p.0));
    }
    siblings.sort();
    siblings.dedup();
    siblings
}

/// Get dependencies of pass. And dependencies of dependencies.
fn dependencies<'a, B, T>(
    passes: &'a [PassBuilder<'a, B, T>],
    pass: &'a PassBuilder<'a, B, T>,
) -> Vec<usize>
where
    B: Backend,
{
    let mut deps = direct_dependencies(passes, pass);
    deps = deps.into_iter()
        .flat_map(|dep| dependencies(passes, &passes[dep]))
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

/*
/// Get dependencies of pass that aren't dependency of dependency.
fn linear_dependencies<'a, B, T>(
    passes: &'a [&'a PassBuilder<'a, B, T>],
    pass: &'a PassBuilder<'a, B, T>,
) -> Vec<&'a PassBuilder<'a, B, T>>
where
    B: Backend,
{
    let mut alldeps = direct_dependencies(passes, pass);
    let mut newdeps = vec![];
    while let Some(dep) = alldeps.pop() {
        newdeps.push(dep);
        let other = dependencies(passes, dep);
        alldeps.retain(|dep| indices_in_of(&other, &[dep]).is_none());
        newdeps.retain(|dep| indices_in_of(&other, &[dep]).is_none());
    }
    newdeps
}
*/