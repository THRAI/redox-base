use driver_graphics::kms::connector::KmsConnectorStatus;
use driver_graphics::kms::objects::{
    KmsCrtc, KmsCrtcDriver, KmsCrtcState, KmsObjectId, KmsObjects, KmsPlane, KmsPlaneDriver,
    KmsPlaneState,
};
use driver_graphics::{Buffer, Damage, DumbBufferConfig, GraphicsAdapter};

use super::buffer::GpuBuffer;
use super::Device;

#[derive(Debug)]
pub struct Crtc {
    pub pipe_idx: usize,
}

#[derive(Debug)]
pub struct Plane {
    pub pipe_idx: usize,
    pub plane_idx: usize,
}

impl KmsCrtcDriver for Crtc {
    type State = ();
}

impl KmsPlaneDriver for Plane {
    type State = ();
}

impl Buffer for GpuBuffer {
    fn size(&self) -> usize {
        self.size as usize
    }
}

impl GraphicsAdapter for Device {
    type Connector = ();
    type Crtc = Crtc;
    type Plane = Plane;

    type Buffer = GpuBuffer;
    type Framebuffer = ();

    fn name(&self) -> &'static [u8] {
        b"ihdgd"
    }

    fn desc(&self) -> &'static [u8] {
        b"Intel HD Graphics"
    }

    fn init(&mut self, objects: &mut KmsObjects<Self>) {
        self.init_inner(objects);
    }

    fn get_unique(&self) -> String {
        self.unique.clone()
    }

    fn min_max_fb_size(&self) -> (u32, u32, u32, u32) {
        (0, 16384, 0, 16384)
    }

    fn dumb_buffer_config(&self) -> Option<DumbBufferConfig> {
        Some(DumbBufferConfig {
            preferred_depth: 24,
            prefer_shadow: true,
        })
    }

    fn cursor_size(&self) -> Option<(u64, u64)> {
        None
    }

    fn probe_connector(&mut self, objects: &mut KmsObjects<Self>, id: KmsObjectId) {
        let mut connector = objects.get_connector(id).unwrap().lock().unwrap();
        connector.connection = KmsConnectorStatus::Connected;
        // FIXME fetch EDID
    }

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> (Self::Buffer, u32) {
        GpuBuffer::alloc_dumb(&self.gm, &mut self.ggtt, width, height).unwrap()
    }

    fn map_dumb_buffer(&mut self, buffer: &Self::Buffer) -> *mut u8 {
        buffer.virt
    }

    fn create_framebuffer(&mut self, _buffer: &Self::Buffer) -> Self::Framebuffer {
        ()
    }

    fn set_crtc(
        &mut self,
        _objects: &KmsObjects<Self>,
        crtc: &KmsCrtc<Self>,
        state: KmsCrtcState<Self>,
    ) -> syscall::Result<()> {
        *crtc.state.lock().unwrap() = state;
        Ok(())
    }

    fn set_plane(
        &mut self,
        objects: &KmsObjects<Self>,
        plane: &KmsPlane<Self>,
        new_plane_state: KmsPlaneState<Self>,
        _damage: Damage,
    ) -> syscall::Result<()> {
        let buffer = new_plane_state
            .fb_id
            .map(|fb_id| objects.get_framebuffer_maybe_closed(fb_id))
            .transpose()?;

        *plane.state.lock().unwrap() = new_plane_state;

        if let Some(plane_hw) = self.pipes[plane.driver_data.pipe_idx]
            .planes
            .get_mut(plane.driver_data.plane_idx)
        {
            plane_hw.set_framebuffer(buffer);
        }

        Ok(())
    }
}
