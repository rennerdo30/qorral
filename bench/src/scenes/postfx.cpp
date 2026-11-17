// Scene 4: the instanced scene at a lower count rendered to RGBA16F, then a bloom chain (threshold,
// 6 downsample and 5 upsample passes), ACES tonemapping to an LDR target and FXAA to the backbuffer.
// Bandwidth- and fill-rate-heavy.
#include "scenes/scenes.h"

#include <algorithm>

namespace {

constexpr uint32_t kPostfxInstances = 80000;
constexpr int kBloomLevels = 6;  // 1/2 .. 1/64 of the screen

struct PostPush {
    float params[4];
};

class PostfxScene : public Scene {
public:
    explicit PostfxScene(bool fxaa) : fxaa_(fxaa) {}
    const char* name() const override { return "postfx"; }

    void init(gfx::Device& dev) override {
        renderer_.init(dev, persistent, kPostfxInstances);
        scenePipeline_ = renderer_.createPipeline(dev, persistent, false, 1, true);

        gfx::PipelineDesc pd;
        pd.vs = "fullscreen.vert";
        pd.constBytes = sizeof(PostPush);
        pd.res[0] = gfx::Res::Texture;
        pd.backbuffer = false;
        pd.color = gfx::Format::RGBA16F;
        pd.fs = "bloom_down.frag";
        downPipeline_ = owned(persistent, dev, dev.createPipeline(pd));
        pd.fs = "bloom_up.frag";
        pd.additive = true;
        upPipeline_ = owned(persistent, dev, dev.createPipeline(pd));
        pd.fs = "post_tonemap.frag";
        pd.additive = false;
        pd.color = gfx::Format::RGBA8;
        pd.res[1] = gfx::Res::Texture;
        tonemapPipeline_ = owned(persistent, dev, dev.createPipeline(pd));
        pd.fs = "fxaa.frag";
        pd.res[1] = gfx::Res::None;
        pd.backbuffer = true;
        fxaaPipeline_ = owned(persistent, dev, dev.createPipeline(pd));
    }

    void createSized(gfx::Device& dev) override {
        const uint32_t w = dev.width(), h = dev.height();
        hdr_ = owned(sized, dev, dev.createTexture(gfx::Format::RGBA16F, w, h, 1, true));
        depth_ = owned(sized, dev, dev.createDepth(w, h, 1));
        for (int i = 0; i < kBloomLevels; ++i)
            bloom_[i] = owned(sized, dev, dev.createTexture(gfx::Format::RGBA16F, std::max(1u, w >> (i + 1)),
                                                            std::max(1u, h >> (i + 1)), 1, true));
        ldr_ = owned(sized, dev, dev.createTexture(gfx::Format::RGBA8, w, h, 1, true));
    }

    void fullscreen(gfx::Device& dev, gfx::Pipeline pipe, gfx::Texture a, gfx::Texture b, const PostPush& pc) {
        dev.setPipeline(pipe);
        dev.setTexture(0, a);
        if (b) dev.setTexture(1, b);
        dev.setConstants(&pc, sizeof pc);
        dev.draw(3);
    }

    void record(gfx::Device& dev, const FrameInfo& fi) override {
        const uint32_t w = dev.width(), h = dev.height();
        renderer_.update(dev, fi.time + 40.0, float(w) / float(h));  // other viewpoint than "instanced"
        gfx::PassDesc scene;
        scene.color = hdr_;
        scene.depth = depth_;
        const float clear[4] = {0.004f, 0.006f, 0.012f, 1.0f};
        std::copy(clear, clear + 4, scene.clear);
        dev.beginPass(scene);
        renderer_.draw(dev, scenePipeline_);
        dev.endPass();

        // Downsample chain; the first pass applies the soft threshold.
        for (int i = 0; i < kBloomLevels; ++i) {
            gfx::Texture src = i == 0 ? hdr_ : bloom_[i - 1];
            PostPush pc{{1.0f / dev.textureWidth(src), 1.0f / dev.textureHeight(src), 1.0f, i == 0 ? 1.0f : 0.0f}};
            gfx::PassDesc p;
            p.color = bloom_[i];
            p.load = gfx::Load::DontCare;
            dev.beginPass(p);
            fullscreen(dev, downPipeline_, src, {}, pc);
            dev.endPass();
        }
        // Upsample chain: add the blurred smaller level onto the next larger one.
        for (int i = kBloomLevels - 2; i >= 0; --i) {
            gfx::Texture src = bloom_[i + 1];
            PostPush pc{{1.0f / dev.textureWidth(src), 1.0f / dev.textureHeight(src), 1.0f, 0.0f}};
            gfx::PassDesc p;
            p.color = bloom_[i];
            p.load = gfx::Load::Load;
            dev.beginPass(p);
            fullscreen(dev, upPipeline_, src, {}, pc);
            dev.endPass();
        }

        gfx::PassDesc tone;
        tone.color = ldr_;
        tone.load = gfx::Load::DontCare;
        dev.beginPass(tone);
        fullscreen(dev, tonemapPipeline_, hdr_, bloom_[0], PostPush{{1.0f, 0.35f, 0.0f, 0.0f}});  // exposure, bloom
        dev.endPass();

        gfx::PassDesc out;
        out.load = gfx::Load::DontCare;
        dev.beginPass(out);
        fullscreen(dev, fxaaPipeline_, ldr_, {}, PostPush{{1.0f / w, 1.0f / h, fxaa_ ? 1.0f : 0.0f, 0.0f}});
        dev.endPass();
    }

private:
    bool fxaa_;
    InstancedRenderer renderer_;
    gfx::Pipeline scenePipeline_, downPipeline_, upPipeline_, tonemapPipeline_, fxaaPipeline_;
    gfx::Texture hdr_, depth_, ldr_, bloom_[kBloomLevels];
};

}  // namespace

std::unique_ptr<Scene> makePostfxScene(bool fxaa) { return std::make_unique<PostfxScene>(fxaa); }
