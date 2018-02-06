
#![deny(unused_must_use)]
#![allow(dead_code)]

extern crate genmesh;
extern crate smallvec;
extern crate xfg_examples;
use xfg_examples::*;

use std::borrow::Borrow;
use std::sync::Arc;

use cgmath::{Deg, PerspectiveFov, Transform, Matrix4, EuclideanSpace, Point3};

use gfx_hal::{Backend, Device, IndexType};
use gfx_hal::buffer::{IndexBufferView, Usage};
use gfx_hal::command::{ClearColor, ClearDepthStencil, CommandBuffer, RenderPassInlineEncoder, Primary};
use gfx_hal::device::ShaderError;
use gfx_hal::format::Format;
use gfx_hal::memory::{cast_slice, Pod};
use gfx_hal::pso::{DescriptorSetLayoutBinding, DescriptorSetWrite, DescriptorType, DescriptorWrite, Element, ElemStride, EntryPoint, GraphicsShaderSet, ShaderStageFlags, VertexBufferSet};
use gfx_hal::queue::Transfer;
use gfx_mem::{Block, Factory, SmartAllocator};
use smallvec::SmallVec;
use xfg::{DescriptorPool, Pass, ColorAttachment, DepthStencilAttachment, GraphBuilder};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct VertexArgs {
    proj: Matrix4<f32>,
    view: Matrix4<f32>,
    model: Matrix4<f32>,
}

unsafe impl Pod for VertexArgs {}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PosNormal {
    position: [f32; 3],
    normal: [f32; 3],
}

unsafe impl Pod for PosNormal {}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PointLight {
    color: [f32; 4],
    position: [f32; 3],
    pad: f32,
}

unsafe impl Pod for PointLight {}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ObjectData {
    albedo: [f32; 3],
    metallic: f32,
    emission: [f32; 3],
    roughness: f32,
    ambient_occlusion: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct FragmentArgs {
    plight: [PointLight; 32],
    camera_position: [f32; 3],
    point_light_count: u32,
    ambient_light: [f32; 3],
    ambient_occlusion: f32,
    albedo: [f32; 3],
    metallic: f32,
    emission: [f32; 3],
    roughness: f32,
}

unsafe impl Pod for FragmentArgs {}

fn pad(value: [f32; 3], pad: f32) -> [f32; 4] {
    [value[0], value[1], value[2], pad]
}

#[derive(Debug)]
struct DrawPbm;

impl<B> Pass<B, Scene<B, ObjectData>> for DrawPbm
where
    B: Backend,
{
    /// Name of the pass
    fn name(&self) -> &str {
        "DrawPbm"
    }

    /// Input attachments
    fn inputs(&self) -> usize { 0 }

    /// Color attachments
    fn colors(&self) -> usize { 1 }

    /// Uses depth attachment
    fn depth(&self) -> bool { true }

    /// Uses stencil attachment
    fn stencil(&self) -> bool { false }

    /// Vertices format
    fn vertices(&self) -> &[(&[Element<Format>], ElemStride)] {
        &[
            (
                &[
                    Element {
                        format: Format::Rgb32Float,
                        offset: 0,
                    },
                    Element {
                        format: Format::Rgb32Float,
                        offset: 12,
                    },
                ],
                24,
            )
        ]
    }

    fn bindings(&self) -> &[DescriptorSetLayoutBinding] {
        &[
            DescriptorSetLayoutBinding {
                binding: 0,
                ty: DescriptorType::UniformBuffer,
                count: 1,
                stage_flags: ShaderStageFlags::VERTEX,
            },

            DescriptorSetLayoutBinding {
                binding: 1,
                ty: DescriptorType::UniformBuffer,
                count: 1,
                stage_flags: ShaderStageFlags::FRAGMENT,
            },
        ]
    }

    fn shaders<'a>(
        &self,
        shaders: &'a mut SmallVec<[B::ShaderModule; 5]>,
        device: &B::Device,
    ) -> Result<GraphicsShaderSet<'a, B>, ShaderError> {
        shaders.clear();
        shaders.push(device.create_shader_module(include_bytes!("vert.spv"))?);
        shaders.push(device.create_shader_module(include_bytes!("frag.spv"))?);

        Ok(GraphicsShaderSet {
            vertex: EntryPoint {
                entry: "main",
                module: &shaders[0],
                specialization: &[],
            },
            hull: None,
            domain: None,
            geometry: None,
            fragment: Some(EntryPoint {
                entry: "main",
                module: &shaders[1],
                specialization: &[],
            }),
        })
    }

    fn prepare<'a>(
        &mut self,
        pool: &mut DescriptorPool<B>,
        cbuf: &mut CommandBuffer<B, Transfer>,
        device: &B::Device,
        _inputs: &[&B::Image],
        frame: usize,
        scene: &mut Scene<B, ObjectData>,
    )
    {
        let ref mut allocator = scene.allocator;
        let view = scene.camera.transform.inverse_transform().unwrap();
        let camera_position = scene.camera.transform.transform_point(Point3::origin()).into();
        // Update uniform cache
        for obj in &mut scene.objects {
            let vertex_args = VertexArgs {
                model: obj.transform,
                proj: scene.camera.projection,
                view,
            };

            let mut plight: [PointLight; 32] = unsafe { ::std::mem::zeroed() };
            let mut point_light_count = 0;

            for light in &scene.lights {
                plight[point_light_count].position = light.transform.transform_point(Point3::origin()).into();
                plight[point_light_count].color = pad(light.color, 1.0);
                point_light_count += 1;
            }

            let fragment_args = FragmentArgs {
                plight,
                camera_position,
                point_light_count: point_light_count as u32,
                ambient_light: scene.ambient.0,
                ambient_occlusion: obj.data.ambient_occlusion,
                albedo: obj.data.albedo,
                metallic: obj.data.metallic,
                emission: obj.data.emission,
                roughness: obj.data.roughness,
            };
            
            let vertex_args_size = ::std::mem::size_of::<VertexArgs>() as u64;
            let fragment_args_size = ::std::mem::size_of::<FragmentArgs>() as u64;
            let size = vertex_args_size + fragment_args_size;

            let grow = (obj.cache.len() .. frame + 1).map(|_| None);
            obj.cache.extend(grow);            
            let cache = obj.cache[frame].get_or_insert_with(|| {
                let buffer = allocator.create_buffer(device, REQUEST_DEVICE_LOCAL, size, Usage::UNIFORM).unwrap();
                let set = pool.allocate(device);
                device.update_descriptor_sets(&[
                    DescriptorSetWrite {
                        set: &set,
                        binding: 0,
                        array_offset: 0,
                        write: DescriptorWrite::UniformBuffer(vec![
                            (buffer.borrow(), 0 .. vertex_args_size)
                        ]),
                    },
                    DescriptorSetWrite {
                        set: &set,
                        binding: 1,
                        array_offset: 0,
                        write: DescriptorWrite::UniformBuffer(vec![
                            (buffer.borrow(), vertex_args_size .. size)
                        ]),
                    },
                ]);
                Cache {
                    uniforms: vec![buffer],
                    views: Vec::new(),
                    set,
                }
            });
            cbuf.update_buffer(cache.uniforms[0].borrow(), 0, cast_slice(&[vertex_args]));
            cbuf.update_buffer(cache.uniforms[0].borrow(), vertex_args_size, cast_slice(&[fragment_args]));
        }
    }

    fn draw_inline<'a>(
        &mut self,
        layout: &B::PipelineLayout,
        mut encoder: RenderPassInlineEncoder<B, Primary>,
        _device: &B::Device,
        _inputs: &[&B::Image],
        frame: usize,
        scene: &Scene<B, ObjectData>,
    ) {
        for object in &scene.objects {
            encoder.bind_graphics_descriptor_sets(layout, 0, Some(&object.cache[frame].as_ref().unwrap().set));
            encoder.bind_index_buffer(IndexBufferView {
                buffer: object.mesh.indices.borrow(),
                offset: 0,
                index_type: IndexType::U16,
            });
            encoder.bind_vertex_buffers(VertexBufferSet(vec![(object.mesh.vertices.borrow(), 0)]));
            encoder.draw_indexed(
                0 .. object.mesh.index_count,
                0,
                0 .. 1,
            );
        }
    }

    fn cleanup(&mut self, pool: &mut DescriptorPool<B>, device: &B::Device, scene: &mut Scene<B, ObjectData>) {
        for object in &mut scene.objects {
            for cache in object.cache.drain(..) {
                if let Some(cache) = cache {
                    pool.free(cache.set);
                    for uniform in cache.uniforms {
                        scene.allocator.destroy_buffer(device, uniform);
                    }
                }
            }
        }
    }
}

fn graph<'a, B>(surface_format: Format, colors: &'a mut Vec<ColorAttachment>, depths: &'a mut Vec<DepthStencilAttachment>) -> GraphBuilder<'a, B, Scene<B, ObjectData>>
where
    B: Backend,
{
    colors.push(ColorAttachment::new(surface_format).with_clear(ClearColor::Float([0.0, 0.0, 0.0, 1.0])));
    depths.push(DepthStencilAttachment::new(Format::D32Float).with_clear(ClearDepthStencil(1.0, 0)));

    let pass = DrawPbm.build()
        .with_color(colors.last().unwrap())
        .with_depth_stencil(depths.last().unwrap());

    GraphBuilder::new()
        .with_pass(pass)
        .with_present(colors.last().unwrap())
}

fn fill<B>(scene: &mut Scene<B, ObjectData>, device: &B::Device)
where
    B: Backend,
{
    scene.camera.transform = Matrix4::from_translation([0.0, 0.0, 15.0].into());

    let mut data = ObjectData {
        albedo: [1.0; 3],
        metallic: 0.0,
        emission: [0.0, 0.0, 0.0],
        roughness: 0.0,
        ambient_occlusion: 1.0,
    };

    let sphere = Arc::new(create_sphere(device, &mut scene.allocator));

    for i in 0 .. 6 {
        for j in 0 .. 6 {
            let transform = Matrix4::from_translation([2.5 * (i as f32) - 6.25, 2.5 * (j as f32) - 6.25, 0.0].into());
            data.metallic = j as f32 * 0.2;
            data.roughness = i as f32  * 0.2;
            scene.objects.push(Object {
                mesh: sphere.clone(),
                data,
                transform,
                cache: Vec::new(),
            });
        }
    }

    scene.lights.push(
        Light {
            color: [0.0, 0.623529411764706, 0.419607843137255],
            transform: Matrix4::from_translation([-6.25, -6.25, 10.0].into()),
            cache: Vec::new(),
        }
    );

    scene.lights.push(
        Light {
            color: [0.768627450980392, 0.007843137254902, 0.2],
            transform: Matrix4::from_translation([6.25, -6.25, 10.0].into()),
            cache: Vec::new(),
        }
    );

    scene.lights.push(
        Light {
            color: [1.0, 0.827450980392157, 0.0],
            transform: Matrix4::from_translation([-6.25, 6.25, 10.0].into()),
            cache: Vec::new(),
        }
    );

    scene.lights.push(
        Light {
            color: [0.0, 0.529411764705882, 0.741176470588235],
            transform: Matrix4::from_translation([6.25, 6.25, 10.0].into()),
            cache: Vec::new(),
        }
    );
}

fn main() {
    run(graph::<back::Backend>, fill);
}

fn create_sphere<B>(device: &B::Device, factory: &mut SmartAllocator<B>) -> Mesh<B>
where
    B: Backend,
{
    use genmesh::{EmitTriangles, Polygon, Triangle};
    use genmesh::generators::{SphereUV, SharedVertex, IndexedPolygon};

    let sphere = SphereUV::new(40, 20);

    let vertices = sphere.shared_vertex_iter().map(|v| {
        PosNormal {
            position: v.pos,
            normal: v.normal,
        }
    }).collect::<Vec<_>>();

    let vertices: &[u8] = cast_slice(&vertices);

    let buffer = factory.create_buffer(device, REQUEST_CPU_VISIBLE, vertices.len() as u64, Usage::VERTEX).unwrap();
    {
        let start = buffer.range().start;
        let end = start + vertices.len() as u64;
        let mut writer = device.acquire_mapping_writer(buffer.memory(), start .. end).unwrap();
        writer.copy_from_slice(vertices);
        device.release_mapping_writer(writer);
    }

    let vertices = buffer;

    let indices = sphere.indexed_polygon_iter().flat_map(|polygon| {
        let mut indices = SmallVec::<[u16; 6]>::new();
        polygon.emit_triangles(|Triangle {x, y, z}| {
           indices.push(x as u16);
           indices.push(y as u16);
           indices.push(z as u16); 
        });
        indices
    }).collect::<Vec<_>>();

    let index_count = indices.len() as u32;

    let indices: &[u8] = cast_slice(&indices);

    let buffer = factory.create_buffer(device, REQUEST_CPU_VISIBLE, indices.len() as u64, Usage::INDEX).unwrap();
    {
        let mut writer = device.acquire_mapping_writer(buffer.memory(), buffer.range()).unwrap();
        writer.copy_from_slice(indices);
        device.release_mapping_writer(writer);
    }

    let indices = buffer;

    Mesh {
        vertices,
        indices,
        index_count,
    }
}
