use common::{ipc::PixelFormat, mmap::Mmap};
use waybackend::{objman::ObjectManager, types::ObjectId, Waybackend};

use crate::WaylandObject;

#[derive(Debug)]
struct Buffer {
    object_id: ObjectId,
    released: bool,
}

impl Buffer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        pool_id: ObjectId,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: super::wl_shm::Format,
    ) -> Self {
        let object_id = objman.create(WaylandObject::Buffer);
        super::wl_shm_pool::req::create_buffer(
            backend, pool_id, object_id, offset, width, height, stride, format,
        )
        .expect("WlShmPool failed to create buffer");
        Self {
            object_id,
            released: true,
        }
    }

    fn is_released(&self) -> bool {
        self.released
    }

    pub fn set_released(&mut self) {
        self.released = true;
    }

    fn unset_released(&mut self) {
        self.released = false;
    }

    fn destroy(self, backend: &mut Waybackend) {
        if let Err(e) = super::wl_buffer::req::destroy(backend, self.object_id) {
            log::error!("failed to destroy wl_buffer: {e:?}");
        }
    }
}

#[derive(Debug)]
/// A pool implementation that only gives buffers of a fixed size, creating new ones if none of
/// them are freed. It also takes care of copying the previous buffer's content over to the new one
/// for us.
///
/// Current implementation will automatically unmap the underlying shared memory when we aren't
/// animating and all created buffers have been released
pub(crate) struct BumpPool {
    pool_id: ObjectId,
    mmap: Mmap,
    buffers: Vec<Buffer>,
    width: i32,
    height: i32,
    last_used_buffer: usize,
}

impl BumpPool {
    /// We assume `width` and `height` have already been multiplied by their scale factor
    pub(crate) fn new(
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        shm: ObjectId,
        width: i32,
        height: i32,
        pixel_format: PixelFormat,
    ) -> Self {
        let len = width as usize * height as usize * pixel_format.channels() as usize;
        let mmap = Mmap::create(len);
        let pool_id = objman.create(WaylandObject::ShmPool);
        super::wl_shm::req::create_pool(backend, shm, pool_id, &mmap.fd(), len as i32)
            .expect("failed to create WlShmPool object");
        let buffers = Vec::with_capacity(2);

        Self {
            pool_id,
            mmap,
            buffers,
            width,
            height,
            last_used_buffer: 0,
        }
    }

    /// Releases a buffer, if we have it
    ///
    /// This will unmap the underlying shared memory if we aren't animating and all buffers have
    /// been released
    pub(crate) fn set_buffer_release_flag(
        &mut self,
        backend: &mut Waybackend,
        buffer_id: ObjectId,
        is_animating: bool,
    ) -> bool {
        if let Some(b) = self.buffers.iter_mut().find(|b| b.object_id == buffer_id) {
            b.set_released();
            if !is_animating && self.buffers.iter().all(|b| b.is_released()) {
                for buffer in self.buffers.drain(..) {
                    buffer.destroy(backend);
                }
                self.mmap.unmap();
            }
            true
        } else {
            false
        }
    }

    const fn buffer_len(&self, pixel_format: PixelFormat) -> usize {
        self.width as usize * self.height as usize * pixel_format.channels() as usize
    }

    const fn buffer_offset(&self, buffer_index: usize, pixel_format: PixelFormat) -> usize {
        self.buffer_len(pixel_format) * buffer_index
    }

    fn occupied_bytes(&self, pixel_format: PixelFormat) -> usize {
        self.buffer_offset(self.buffers.len(), pixel_format)
    }

    /// resizes the pool and creates a new WlBuffer at the next free offset
    fn grow(
        &mut self,
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        pixel_format: PixelFormat,
    ) {
        let len = self.buffer_len(pixel_format);
        let new_len = self.occupied_bytes(pixel_format) + len;

        // we unmap the shared memory file descriptor when animations are done, so here we must
        // ensure the bytes are actually mmaped
        self.mmap.ensure_mapped();

        if new_len > self.mmap.len() {
            if new_len > i32::MAX as usize {
                panic!("Buffers have grown too big. We cannot allocate any more.")
            }
            self.mmap.remap(new_len);
            super::wl_shm_pool::req::resize(backend, self.pool_id, new_len as i32).unwrap();
        }

        let new_buffer_index = self.buffers.len();
        self.buffers.push(Buffer::new(
            backend,
            objman,
            self.pool_id,
            self.buffer_offset(new_buffer_index, pixel_format) as i32,
            self.width,
            self.height,
            self.width * pixel_format.channels() as i32,
            wl_shm_format(pixel_format),
        ));

        log::info!(
            "BumpPool with: {} buffers. Size: {}Kb",
            self.buffers.len(),
            self.mmap.len() / 1024
        );
    }

    /// Returns a drawable surface. If we can't find a free buffer, we request more memory
    ///
    /// This function automatically handles copying the previous buffer over onto the new one
    pub(crate) fn get_drawable(
        &mut self,
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        pixel_format: PixelFormat,
    ) -> &mut [u8] {
        let i = match self
            .buffers
            .iter_mut()
            .enumerate()
            .find(|(_, b)| b.is_released())
        {
            Some((i, _)) => i,
            None => {
                self.grow(backend, objman, pixel_format);
                self.buffers.len() - 1
            }
        };

        let len = self.buffer_len(pixel_format);
        let offset = self.buffer_offset(i, pixel_format);

        if self.last_used_buffer != i {
            let last_offset = self.buffer_offset(self.last_used_buffer, pixel_format);
            self.mmap
                .slice_mut()
                .copy_within(last_offset..last_offset + len, offset);
            self.last_used_buffer = i;
        }

        &mut self.mmap.slice_mut()[offset..offset + len]
    }

    /// gets the last buffer we've drawn to
    pub(crate) fn get_commitable_buffer(&mut self) -> ObjectId {
        let buf = &mut self.buffers[self.last_used_buffer];
        buf.unset_released();
        buf.object_id
    }

    /// We assume `width` and `height` have already been multiplied by their scale factor
    pub(crate) fn resize(&mut self, backend: &mut Waybackend, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        self.last_used_buffer = 0;
        for buffer in self.buffers.drain(..) {
            buffer.destroy(backend);
        }
    }

    pub(crate) fn destroy(&mut self, backend: &mut Waybackend) {
        for buffer in self.buffers.drain(..) {
            buffer.destroy(backend);
        }

        if let Err(e) = super::wl_shm_pool::req::destroy(backend, self.pool_id) {
            log::error!("failed to destroy wl_shm_pool: {e}");
        }
    }
}

const fn wl_shm_format(pixel_format: PixelFormat) -> super::wl_shm::Format {
    use super::wl_shm::Format;
    match pixel_format {
        PixelFormat::Bgr => Format::bgr888,
        PixelFormat::Rgb => Format::rgb888,
        PixelFormat::Xbgr => Format::xbgr8888,
        PixelFormat::Xrgb => Format::xrgb8888,
    }
}
