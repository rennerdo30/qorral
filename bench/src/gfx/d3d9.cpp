// Direct3D 9 backend: not implemented yet (HG-22).
#include "gfx/d3d_common.h"

#include <cstdio>

namespace gfx {

std::unique_ptr<Device> createD3D9(SDL_Window*, const Options&) {
    throw Error("the Direct3D 9 backend is not implemented yet");
}

int listD3D9() {
    std::printf("D3D9 is not implemented yet\n");
    return 1;
}

}  // namespace gfx
