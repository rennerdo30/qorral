// Scene 1: full-screen SDF raymarcher (menger fractal, repeated twisted pillars, blended glowing
// orbs, terrain) with soft shadows, ambient occlusion, one reflection bounce and fog. ALU-heavy.
#include "scenes/scenes.h"

namespace {

struct RaymarchPush {
    float res[4];     // width, height, 1/width, 1/height
    float params[4];  // time
};

class RaymarchScene : public Scene {
public:
    const char* name() const override { return "raymarch"; }

    void init(gfx::Device& dev) override {
        gfx::PipelineDesc pd;
        pd.vs = "fullscreen.vert";
        pd.fs = "raymarch.frag";
        pd.constBytes = sizeof(RaymarchPush);
        pipeline_ = owned(persistent, dev, dev.createPipeline(pd));
    }

    void createSized(gfx::Device&) override {}

    void record(gfx::Device& dev, const FrameInfo& fi) override {
        const float w = float(dev.width()), h = float(dev.height());
        RaymarchPush pc{{w, h, 1.0f / w, 1.0f / h}, {float(fi.time), 0.0f, 0.0f, 0.0f}};
        gfx::PassDesc p;
        p.load = gfx::Load::DontCare;  // every pixel is written
        dev.beginPass(p);
        dev.setPipeline(pipeline_);
        dev.setConstants(&pc, sizeof pc);
        dev.draw(3);
        dev.endPass();
    }

private:
    gfx::Pipeline pipeline_;
};

}  // namespace

std::unique_ptr<Scene> makeRaymarchScene() { return std::make_unique<RaymarchScene>(); }
