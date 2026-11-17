// Direct3D 12 backend: not implemented yet (HG-22).
#include "gfx/d3d_common.h"

namespace gfx {

std::unique_ptr<Device> createD3D12(SDL_Window*, const Options&) {
    throw Error("the Direct3D 12 backend is not implemented yet");
}

}  // namespace gfx
