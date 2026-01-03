//! The [`hudhook`](crate) overlay rendering engine.
mod backend;
mod input;
mod keys;
pub(crate) mod msg_filter;
mod pipeline;

use imgui::internal::RawCast;
use imgui::{sys, Context, DrawData, TextureId};
use windows::core::Result;

use crate::RenderContext;

pub(crate) trait RenderEngine: RenderContext {
    type RenderTarget;

    fn render(&mut self, draw_data: &DrawData, render_target: Self::RenderTarget) -> Result<()>;
    fn setup_fonts(&mut self, ctx: &mut Context) -> Result<()>;
    fn update_textures(&mut self, draw_data: &DrawData) -> Result<()>
    where
        Self: Sized,
    {
        update_textures(self, draw_data)
    }
}

fn update_textures(render_context: &mut dyn RenderContext, draw_data: &DrawData) -> Result<()> {
    let raw_draw_data = unsafe { draw_data.raw() };
    let textures_ptr = raw_draw_data.Textures;
    if textures_ptr.is_null() {
        return Ok(());
    }

    let textures_vec = unsafe { &*textures_ptr };
    if textures_vec.Size <= 0 || textures_vec.Data.is_null() {
        return Ok(());
    }

    let textures = unsafe { std::slice::from_raw_parts(textures_vec.Data, textures_vec.Size as usize) };
    for &tex_ptr in textures {
        if tex_ptr.is_null() {
            continue;
        }
        unsafe {
            let tex = &mut *tex_ptr;
            let status = tex.Status;
            if status == sys::ImTextureStatus_OK || status == sys::ImTextureStatus_Destroyed {
                continue;
            }

            if status == sys::ImTextureStatus_WantDestroy {
                sys::ImTextureData_SetTexID(tex_ptr, 0);
                sys::ImTextureData_SetStatus(tex_ptr, sys::ImTextureStatus_Destroyed);
                continue;
            }

            let width = tex.Width as u32;
            let height = tex.Height as u32;
            if width == 0 || height == 0 || tex.Pixels.is_null() {
                continue;
            }

            let pitch = sys::ImTextureData_GetPitch(tex_ptr) as usize;
            let height_usize = height as usize;
            let bpp = tex.BytesPerPixel as usize;
            let data = std::slice::from_raw_parts(tex.Pixels, pitch * height_usize);

            let mut rgba_data_storage = Vec::new();
            let upload_data = if bpp == 1 {
                rgba_data_storage = Vec::with_capacity((width as usize) * (height as usize) * 4);
                for y in 0..height_usize {
                    let row = &data[(y * pitch)..(y * pitch + width as usize)];
                    for &a in row {
                        rgba_data_storage.extend_from_slice(&[255, 255, 255, a]);
                    }
                }
                rgba_data_storage.as_slice()
            } else {
                data
            };

            let tex_id = tex.TexID;
            let is_invalid = tex_id == 0;
            if status == sys::ImTextureStatus_WantCreate || is_invalid {
                let new_id = render_context.load_texture(upload_data, width, height)?;
                sys::ImTextureData_SetTexID(tex_ptr, new_id.id() as sys::ImTextureID);
                sys::ImTextureData_SetStatus(tex_ptr, sys::ImTextureStatus_OK);
                continue;
            }

            if status == sys::ImTextureStatus_WantUpdates {
                let existing_id = TextureId::from(tex_id as usize);
                render_context.replace_texture(existing_id, upload_data, width, height)?;
                sys::ImTextureData_SetStatus(tex_ptr, sys::ImTextureStatus_OK);
            }
        }
    }

    Ok(())
}
#[cfg(feature = "dx11")]
pub(crate) use backend::dx11::D3D11RenderEngine;
#[cfg(feature = "dx12")]
pub(crate) use backend::dx12::D3D12RenderEngine;
#[cfg(feature = "dx9")]
pub(crate) use backend::dx9::D3D9RenderEngine;
#[cfg(feature = "opengl3")]
pub(crate) use backend::opengl3::OpenGl3RenderEngine;
pub(crate) use pipeline::Pipeline;
