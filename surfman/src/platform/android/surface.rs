// surfman/src/platform/android/surface.rs

//! Surface management for Android using the `GraphicBuffer` class and
//! EGL.

use crate::context::ContextID;
use crate::egl::types::{EGLClientBuffer, EGLImageKHR, EGLSurface, EGLint};
use crate::gl::Gl;
use crate::gl::types::{GLenum, GLint, GLuint};
use crate::renderbuffers::Renderbuffers;
use crate::{Error, SurfaceID, egl, gl};
use super::context::{Context, GL_FUNCTIONS};
use super::device::{Device, EGL_EXTENSION_FUNCTIONS};

use android_ndk_sys::{ANativeWindow, ANativeWindow_getHeight, ANativeWindow_getWidth};
use euclid::default::Size2D;
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;
use std::thread;

// FIXME(pcwalton): Is this right, or should it be `TEXTURE_EXTERNAL_OES`?
const SURFACE_GL_TEXTURE_TARGET: GLenum = gl::TEXTURE_2D;

pub struct Surface {
    pub(crate) context_id: ContextID,
    pub(crate) size: Size2D<i32>,
    pub(crate) objects: SurfaceObjects,
    pub(crate) destroyed: bool,
}

pub struct SurfaceTexture {
    pub(crate) surface: Surface,
    pub(crate) texture_object: GLuint,
    pub(crate) phantom: PhantomData<*const ()>,
}

pub(crate) enum SurfaceObjects {
    EGLImage {
        egl_image: EGLImageKHR,
        framebuffer_object: GLuint,
        texture_object: GLuint,
        renderbuffers: Renderbuffers,
    },
    Window {
        egl_surface: EGLSurface,
    },
}

unsafe impl Send for Surface {}

impl Debug for Surface {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "Surface({:x})", self.id().0)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if !self.destroyed && !thread::panicking() {
            panic!("Should have destroyed the surface first with `destroy_surface()`!")
        }
    }
}

pub enum SurfaceType {
    Generic { size: Size2D<i32> },
    Widget { native_widget: NativeWidget },
}

pub struct NativeWidget {
    pub(crate) native_window: *mut ANativeWindow,
}

impl Device {
    pub fn create_surface(&mut self, context: &Context, surface_type: &SurfaceType)
                          -> Result<Surface, Error> {
        match *surface_type {
            SurfaceType::Generic { ref size } => self.create_generic_surface(context, size),
            SurfaceType::Widget { ref native_widget } => {
                unsafe {
                    self.create_window_surface(context, native_widget.native_window)
                }
            }
        }
    }

    fn create_generic_surface(&mut self, context: &Context, size: &Size2D<i32>)
                              -> Result<Surface, Error> {
        GL_FUNCTIONS.with(|gl| {
            unsafe {
                // Initialize the texture.
                let mut texture_object = 0;
                gl.GenTextures(1, &mut texture_object);
                gl.BindTexture(gl::TEXTURE_2D, texture_object);
                gl.TexImage2D(gl::TEXTURE_2D,
                              0,
                              gl::RGBA as GLint,
                              size.width,
                              size.height,
                              0,
                              gl::RGBA,
                              gl::UNSIGNED_BYTE,
                              ptr::null());
                self.set_texture_parameters(gl);
                gl.BindTexture(gl::TEXTURE_2D, 0);

                // Create an EGL image, and bind it to a texture.
                let egl_image = self.create_egl_image(context, texture_object);

                let mut framebuffer_object = 0;
                gl.GenFramebuffers(1, &mut framebuffer_object);
                gl.BindFramebuffer(gl::FRAMEBUFFER, framebuffer_object);

                gl.FramebufferTexture2D(gl::FRAMEBUFFER,
                                        gl::COLOR_ATTACHMENT0,
                                        SURFACE_GL_TEXTURE_TARGET,
                                        texture_object,
                                        0);

                let context_descriptor = self.context_descriptor(context);
                let context_attributes = self.context_descriptor_attributes(&context_descriptor);

                let renderbuffers = Renderbuffers::new(size, &context_attributes);
                renderbuffers.bind_to_current_framebuffer();

                debug_assert_eq!(gl.CheckFramebufferStatus(gl::FRAMEBUFFER),
                                 gl::FRAMEBUFFER_COMPLETE);

                Ok(Surface {
                    size: *size,
                    context_id: context.id,
                    objects: SurfaceObjects::EGLImage {
                        egl_image,
                        framebuffer_object,
                        texture_object,
                        renderbuffers,
                    },
                    destroyed: false,
                })
            }
        })
    }

    unsafe fn create_window_surface(&mut self,
                                    context: &Context,
                                    native_window: *mut ANativeWindow)
                                    -> Result<Surface, Error> {
        let width = ANativeWindow_getWidth(native_window);
        let height = ANativeWindow_getHeight(native_window);

        let context_descriptor = self.context_descriptor(context);
        let egl_config = self.context_descriptor_to_egl_config(&context_descriptor);

        let egl_surface = egl::CreateWindowSurface(self.native_display.egl_display(),
                                                   egl_config,
                                                   native_window as *const c_void,
                                                   ptr::null());
        assert_ne!(egl_surface, egl::NO_SURFACE);

        Ok(Surface {
            context_id: context.id,
            size: Size2D::new(width, height),
            objects: SurfaceObjects::Window { egl_surface },
            destroyed: false,
        })
    }

    pub fn create_surface_texture(&self, _: &mut Context, surface: Surface)
                                  -> Result<SurfaceTexture, Error> {
        unsafe {
            let texture_object = match surface.objects {
                SurfaceObjects::Window { .. } => return Err(Error::WidgetAttached),
                SurfaceObjects::EGLImage { egl_image, .. } => self.bind_to_gl_texture(egl_image),
            };
            Ok(SurfaceTexture { surface, texture_object, phantom: PhantomData })
        }
    }

    pub fn present_surface(&self, _: &Context, surface: &mut Surface) -> Result<(), Error> {
        self.present_surface_without_context(surface)
    }

    pub(crate) fn present_surface_without_context(&self, surface: &mut Surface)
                                                  -> Result<(), Error> {
        unsafe {
            match surface.objects {
                SurfaceObjects::Window { egl_surface } => {
                    egl::SwapBuffers(self.native_display.egl_display(), egl_surface);
                    Ok(())
                }
                SurfaceObjects::EGLImage { .. } => Err(Error::NoWidgetAttached),
            }
        }
    }

    unsafe fn create_egl_image(&self, context: &Context, texture_object: GLuint) -> EGLImageKHR {
        // Create the EGL image.
        let egl_image_attributes = [
            egl::GL_TEXTURE_LEVEL as EGLint,    0,
            egl::IMAGE_PRESERVED_KHR as EGLint, egl::TRUE as EGLint,
            egl::NONE as EGLint,                0,
        ];
        let egl_image = egl::CreateImageKHR(self.native_display.egl_display(),
                                            context.native_context.egl_context(),
                                            egl::GL_TEXTURE_2D,
                                            texture_object as EGLClientBuffer,
                                            egl_image_attributes.as_ptr());
        assert_ne!(egl_image, egl::NO_IMAGE_KHR);
        egl_image
    }

    unsafe fn set_texture_parameters(&self, gl: &Gl) {
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as GLint);
        gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as GLint);
    }

    unsafe fn bind_to_gl_texture(&self, egl_image: EGLImageKHR) -> GLuint {
        GL_FUNCTIONS.with(|gl| {
            let mut texture = 0;
            gl.GenTextures(1, &mut texture);
            debug_assert_ne!(texture, 0);

            gl.BindTexture(gl::TEXTURE_2D, texture);
            (EGL_EXTENSION_FUNCTIONS.ImageTargetTexture2DOES)(gl::TEXTURE_2D, egl_image);
            self.set_texture_parameters(gl);
            gl.BindTexture(gl::TEXTURE_2D, 0);

            debug_assert_eq!(gl.GetError(), gl::NO_ERROR);
            texture
        })
    }

    pub fn destroy_surface(&self, context: &mut Context, mut surface: Surface)
                           -> Result<(), Error> {
        if context.id != surface.context_id {
            // Leak the surface, and return an error.
            surface.destroyed = true;
            return Err(Error::IncompatibleSurface);
        }

        unsafe {
            match surface.objects {
                SurfaceObjects::EGLImage {
                    ref mut egl_image,
                    ref mut framebuffer_object,
                    ref mut texture_object,
                    ref mut renderbuffers,
                } => {
                    GL_FUNCTIONS.with(|gl| {
                        gl.BindFramebuffer(gl::FRAMEBUFFER, 0);
                        gl.DeleteFramebuffers(1, framebuffer_object);
                        *framebuffer_object = 0;
                        renderbuffers.destroy();

                        let result = egl::DestroyImageKHR(self.native_display.egl_display(),
                                                          *egl_image);
                        assert_ne!(result, egl::FALSE);
                        *egl_image = egl::NO_IMAGE_KHR;

                        gl.DeleteTextures(1, texture_object);
                        *texture_object = 0;
                    });
                }
                SurfaceObjects::Window { ref mut egl_surface } => {
                    egl::DestroySurface(self.native_display.egl_display(), *egl_surface);
                    *egl_surface = egl::NO_SURFACE;
                }
            }
        }

        surface.destroyed = true;
        Ok(())
    }

    pub fn destroy_surface_texture(&self, _: &mut Context, mut surface_texture: SurfaceTexture)
                                   -> Result<Surface, Error> {
        GL_FUNCTIONS.with(|gl| {
            unsafe {
                gl.DeleteTextures(1, &surface_texture.texture_object);
                surface_texture.texture_object = 0;
            }

            Ok(surface_texture.surface)
        })
    }

    #[inline]
    pub fn surface_gl_texture_target(&self) -> GLenum {
        SURFACE_GL_TEXTURE_TARGET
    }
}

impl NativeWidget {
    #[inline]
    pub unsafe fn from_native_window(native_window: *mut ANativeWindow) -> NativeWidget {
        NativeWidget { native_window }
    }
}

impl Surface {
    #[inline]
    pub fn size(&self) -> Size2D<i32> {
        self.size
    }

    pub fn id(&self) -> SurfaceID {
        match self.objects {
            SurfaceObjects::EGLImage { egl_image, .. } => SurfaceID(egl_image as usize),
            SurfaceObjects::Window { egl_surface } => SurfaceID(egl_surface as usize),
        }
    }

    #[inline]
    pub fn context_id(&self) -> ContextID {
        self.context_id
    }
}

impl SurfaceTexture {
    #[inline]
    pub fn gl_texture(&self) -> GLuint {
        self.texture_object
    }
}