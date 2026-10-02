//! Draws the mouse cursor into captured frames. Desktop Duplication reports
//! the cursor separately from the desktop image: its position and visibility
//! with every frame, and its shape whenever that changes. The shape becomes a
//! small texture that a tiny shader blends over a copy of the frame.

use crate::prelude::*;

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, IDXGIOutputDuplication,
};
use windows::core::Result;

// Compiled from shaders/cursor.hlsl by shaders/build.ps1.
const VERTEX_SHADER: &[u8] = include_bytes!("../shaders/cursor_vs.cso");
const PIXEL_SHADER: &[u8] = include_bytes!("../shaders/cursor_ps.cso");

struct Shape {
    view: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

pub struct CursorOverlay {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    target: ID3D11RenderTargetView,
    shape: Option<Shape>,
    visible: bool,
    x: i32,
    y: i32,
    buffer: Vec<u8>,
}

impl CursorOverlay {
    /// `target` is the texture the cursor gets drawn onto.
    pub fn new(device: &ID3D11Device, target: &ID3D11Texture2D) -> Result<Self> {
        unsafe {
            let mut vertex_shader = None;
            device.CreateVertexShader(VERTEX_SHADER, None, Some(&mut vertex_shader))?;
            let mut pixel_shader = None;
            device.CreatePixelShader(PIXEL_SHADER, None, Some(&mut pixel_shader))?;

            // Ordinary "over" blending with straight (non-premultiplied) alpha.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&blend_desc, Some(&mut blend))?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

            let mut target_view = None;
            device.CreateRenderTargetView(target, None, Some(&mut target_view))?;

            Ok(Self {
                vertex_shader: vertex_shader.unwrap(),
                pixel_shader: pixel_shader.unwrap(),
                blend: blend.unwrap(),
                sampler: sampler.unwrap(),
                target: target_view.unwrap(),
                shape: None,
                visible: false,
                x: 0,
                y: 0,
                buffer: Vec::new(),
            })
        }
    }

    /// Apply the pointer updates that came with a frame. Must be called
    /// between AcquireNextFrame and ReleaseFrame.
    pub fn update(
        &mut self,
        device: &ID3D11Device,
        duplication: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<()> {
        if info.LastMouseUpdateTime != 0 {
            self.visible = info.PointerPosition.Visible.as_bool();
            // Already the top-left of the cursor image, hotspot included.
            self.x = info.PointerPosition.Position.x;
            self.y = info.PointerPosition.Position.y;
        }
        if info.PointerShapeBufferSize > 0 {
            self.buffer.resize(info.PointerShapeBufferSize as usize, 0);
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut required = 0;
            unsafe {
                duplication.GetFramePointerShape(
                    self.buffer.len() as u32,
                    self.buffer.as_mut_ptr() as _,
                    &mut required,
                    &mut shape,
                )?;
            }
            if let Some((pixels, width, height)) = to_bgra(&shape, &self.buffer) {
                self.shape = Some(upload(device, &pixels, width, height)?);
            }
        }
        Ok(())
    }

    /// Forget the cursor's state, e.g. after the duplication was recreated.
    pub fn reset(&mut self) {
        self.visible = false;
    }

    pub fn visible(&self) -> bool {
        self.visible && self.shape.is_some()
    }

    pub fn draw(&self, context: &ID3D11DeviceContext) {
        let Some(shape) = self.shape.as_ref() else { return };
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(self.target.clone())]), None);
            // The viewport is the cursor rectangle; the shader covers it with
            // one triangle. Parts off the edge of the screen are clipped.
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: self.x as f32,
                TopLeftY: self.y as f32,
                Width: shape.width as f32,
                Height: shape.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetShaderResources(0, Some(&[Some(shape.view.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.OMSetBlendState(&self.blend, None, u32::MAX);
            context.Draw(3, 0);
            // Unbind so the texture can be used as a copy target again.
            context.OMSetRenderTargets(None, None);
        }
    }
}

/// Convert a Desktop Duplication pointer shape to straight-alpha BGRA.
///
/// Cursors that invert the pixels underneath them (the classic text I-beam)
/// can't be reproduced with plain blending, so inverted pixels are drawn
/// black, which reads fine on the light backgrounds text cursors usually sit on.
fn to_bgra(shape: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let (w, pitch) = (shape.Width as usize, shape.Pitch as usize);
    match shape.Type as i32 {
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 => {
            // An AND mask on top of an XOR mask, one bit per pixel.
            let h = shape.Height as usize / 2;
            let bit = |row: usize, x: usize| data.get(row * pitch + x / 8).is_some_and(|b| b & (0x80 >> (x % 8)) != 0);
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let pixel: [u8; 4] = match (bit(y, x), bit(y + h, x)) {
                        (false, false) => [0, 0, 0, 255],      // black
                        (false, true) => [255, 255, 255, 255], // white
                        (true, false) => [0, 0, 0, 0],         // transparent
                        (true, true) => [0, 0, 0, 255],        // inverted
                    };
                    out.extend_from_slice(&pixel);
                }
            }
            Some((out, w as u32, h as u32))
        }
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 || t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => {
            let masked = t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
            let h = shape.Height as usize;
            let mut out = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                let row = data.get(y * pitch..y * pitch + w * 4)?;
                for px in row.chunks_exact(4) {
                    let mut p = [px[0], px[1], px[2], px[3]];
                    if masked {
                        // Alpha is a mask here: 0 = replace the screen pixel,
                        // 0xFF = XOR with it (approximated as the colour itself).
                        let empty = p[..3] == [0, 0, 0];
                        p[3] = if p[3] == 0 || !empty { 255 } else { 0 };
                    }
                    out.extend_from_slice(&p);
                }
            }
            Some((out, w as u32, h as u32))
        }
        _ => None,
    }
}

fn upload(device: &ID3D11Device, pixels: &[u8], width: u32, height: u32) -> Result<Shape> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr() as _, SysMemPitch: width * 4, SysMemSlicePitch: 0 };
    unsafe {
        let mut texture = None;
        device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture))?;
        let mut view = None;
        device.CreateShaderResourceView(texture.as_ref().unwrap(), None, Some(&mut view))?;
        Ok(Shape { view: view.unwrap(), width, height })
    }
}
