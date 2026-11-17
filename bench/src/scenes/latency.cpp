// Latency scene: the whole screen is black, and white for 250 ms after every key or mouse-button
// press. Frames run one at a time (closed loop): each frame waits until it is displayed, which
// gives the in-guest frame latency, and an outside observer (qorral latency) times input to pixels.
#include "scenes/scenes.h"

namespace {

constexpr double kFlashSeconds = 0.25;

class LatencyScene : public Scene {
public:
    const char* name() const override { return "latency"; }
    bool closedLoop() const override { return true; }

    void init(gfx::Device& dev) override {
        gfx::PipelineDesc pd;
        pd.vs = "fullscreen.vert";
        pd.fs = "solid.frag";
        pd.constBytes = 16;
        pipeline_ = owned(persistent, dev, dev.createPipeline(pd));
        lastInputs_ = 0;
        flashUntil_ = -1.0;
    }

    void createSized(gfx::Device&) override {}

    void record(gfx::Device& dev, const FrameInfo& fi) override {
        if (fi.inputs != lastInputs_) {
            lastInputs_ = fi.inputs;
            flashUntil_ = fi.time + kFlashSeconds;
        }
        const float v = fi.time < flashUntil_ ? 1.0f : 0.0f;
        const float color[4] = {v, v, v, 1.0f};
        gfx::PassDesc p;
        p.load = gfx::Load::DontCare;
        dev.beginPass(p);
        dev.setPipeline(pipeline_);
        dev.setConstants(color, sizeof color);
        dev.draw(3);
        dev.endPass();
    }

private:
    gfx::Pipeline pipeline_;
    uint32_t lastInputs_ = 0;
    double flashUntil_ = -1.0;
};

}  // namespace

std::unique_ptr<Scene> makeLatencyScene() { return std::make_unique<LatencyScene>(); }
