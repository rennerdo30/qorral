// Scene 3: 1M particles simulated on the GPU (curl noise plus three moving attractors), drawn as
// additive camera-facing quads into an RGBA16F target, then tonemapped to the backbuffer.
// Compute- and bandwidth-heavy.
//
// APIs with compute shaders simulate in a compute shader on a storage buffer. D3D9 and D3D10 have
// no compute shaders: they run the same simulation in a pixel shader on two RGBA32F 1024x1024
// textures (position and velocity, ping-ponged), and the vertex shader reads the textures.
#include "scenes/scenes.h"

#include <algorithm>
#include <cstring>

namespace {

constexpr uint32_t kParticleCount = 1u << 20;  // 1,048,576
constexpr uint32_t kGroupSize = 256;
constexpr uint32_t kStateSize = 1024;          // texture variant: 1024 x 1024 particles

struct SimPush {
    float time;
    float dt;
    float count;  // floats: Direct3D 9 constant registers hold only floats
    float mode;   // 0 = initialize, 1 = step
    float attractor[3][4];
};

struct DrawPush {
    float viewProj[16];
    float right[4];  // xyz camera right, w particle size
    float up[4];
};

struct CompositePush {
    float params[4];  // exposure
};

class ParticlesScene : public Scene {
public:
    const char* name() const override { return "particles"; }

    void init(gfx::Device& dev) override {
        compute_ = dev.supportsCompute();
        gfx::PipelineDesc dd;
        dd.fs = "particles.frag";
        dd.constBytes = sizeof(DrawPush);
        dd.backbuffer = false;
        dd.color = gfx::Format::RGBA16F;
        dd.additive = true;
        if (compute_) {
            particles_ = owned(persistent, dev, dev.createBuffer(gfx::BufferKind::Storage, size_t(kParticleCount) * 32, nullptr));
            gfx::ComputeDesc cd;
            cd.cs = "particles.comp";
            cd.res[0] = gfx::Res::StorageRW;
            cd.constBytes = sizeof(SimPush);
            simPipeline_ = owned(persistent, dev, dev.createCompute(cd));
            dd.vs = "particles.vert";
            dd.res[0] = gfx::Res::StorageRead;
        } else {
            for (int i = 0; i < 2; ++i) {
                pos_[i] = owned(persistent, dev, dev.createTexture(gfx::Format::RGBA32F, kStateSize, kStateSize, 1, true));
                vel_[i] = owned(persistent, dev, dev.createTexture(gfx::Format::RGBA32F, kStateSize, kStateSize, 1, true));
            }
            gfx::PipelineDesc sd;
            sd.vs = "fullscreen.vert";
            sd.fs = "particles_sim.frag";
            sd.res[0] = gfx::Res::Texture;
            sd.res[1] = gfx::Res::Texture;
            sd.constBytes = sizeof(SimPush);
            sd.backbuffer = false;
            sd.color = gfx::Format::RGBA32F;
            sd.colorCount = 2;
            simPipeline_ = owned(persistent, dev, dev.createPipeline(sd));
            dd.vs = "particles_tex.vert";
            dd.res[0] = gfx::Res::Texture;
            dd.res[1] = gfx::Res::Texture;
        }
        drawPipeline_ = owned(persistent, dev, dev.createPipeline(dd));

        gfx::PipelineDesc cp;
        cp.vs = "fullscreen.vert";
        cp.fs = "composite.frag";
        cp.res[0] = gfx::Res::Texture;
        cp.constBytes = sizeof(CompositePush);
        compositePipeline_ = owned(persistent, dev, dev.createPipeline(cp));
        initialized_ = false;
        current_ = 0;
    }

    void createSized(gfx::Device& dev) override {
        hdr_ = owned(sized, dev, dev.createTexture(gfx::Format::RGBA16F, dev.width(), dev.height(), 1, true));
    }

    void record(gfx::Device& dev, const FrameInfo& fi) override {
        const float t = static_cast<float>(fi.time);
        SimPush sim{};
        sim.time = t;
        sim.dt = std::clamp(fi.dt, 0.0f, 1.0f / 30.0f);
        sim.count = static_cast<float>(kParticleCount);
        sim.mode = initialized_ ? 1.0f : 0.0f;
        for (int i = 0; i < 3; ++i) {
            float ph = i * 2.094f;
            sim.attractor[i][0] = 3.2f * std::sin(t * (0.31f + 0.07f * i) + ph);
            sim.attractor[i][1] = 1.8f * std::sin(t * (0.23f + 0.05f * i) + ph * 1.7f);
            sim.attractor[i][2] = 3.2f * std::cos(t * (0.27f + 0.06f * i) + ph);
            sim.attractor[i][3] = 2.2f + 0.8f * std::sin(t * 0.5f + i);
        }
        initialized_ = true;

        // Simulation.
        if (compute_) {
            dev.storageToCompute();
            dev.setPipeline(simPipeline_);
            dev.setStorage(0, particles_);
            dev.setConstants(&sim, sizeof sim);
            dev.dispatch((kParticleCount + kGroupSize - 1) / kGroupSize);
            dev.storageToVertex();
        } else {
            const int src = current_, dst = 1 - current_;
            gfx::PassDesc p;
            p.color = pos_[dst];
            p.color2 = vel_[dst];
            p.load = gfx::Load::DontCare;
            dev.beginPass(p);
            dev.setPipeline(simPipeline_);
            dev.setTexture(0, pos_[src]);
            dev.setTexture(1, vel_[src]);
            dev.setConstants(&sim, sizeof sim);
            dev.draw(3);
            dev.endPass();
            current_ = dst;
        }

        // Additive particle quads into the HDR target.
        const float a = t * 0.12f;
        Vec3 eye{std::cos(a) * 9.5f, 2.5f + 1.5f * std::sin(t * 0.17f), std::sin(a) * 9.5f};
        Vec3 fwd = normalize(Vec3{0, 0, 0} - eye);
        Vec3 right = normalize(cross(fwd, {0, 1, 0}));
        Vec3 up = cross(right, fwd);
        Mat4 vp = perspective(60.0f * 3.14159265f / 180.0f, float(dev.width()) / float(dev.height()), 0.1f, 100.0f) *
                  lookAt(eye, {0, 0, 0}, {0, 1, 0});
        DrawPush dp{};
        std::memcpy(dp.viewProj, vp.m, sizeof vp.m);
        dp.right[0] = right.x;
        dp.right[1] = right.y;
        dp.right[2] = right.z;
        dp.right[3] = 0.022f;
        dp.up[0] = up.x;
        dp.up[1] = up.y;
        dp.up[2] = up.z;
        gfx::PassDesc hp;
        hp.color = hdr_;
        hp.load = gfx::Load::Clear;
        dev.beginPass(hp);
        dev.setPipeline(drawPipeline_);
        if (compute_) {
            dev.setStorage(0, particles_);
        } else {
            dev.setTexture(0, pos_[current_]);
            dev.setTexture(1, vel_[current_]);
        }
        dev.setConstants(&dp, sizeof dp);
        dev.draw(kParticleCount * 6);
        dev.endPass();

        // Tonemap to the backbuffer.
        CompositePush cp{{2.2f, 0.0f, 0.0f, 0.0f}};
        gfx::PassDesc op;
        op.load = gfx::Load::DontCare;
        dev.beginPass(op);
        dev.setPipeline(compositePipeline_);
        dev.setTexture(0, hdr_);
        dev.setConstants(&cp, sizeof cp);
        dev.draw(3);
        dev.endPass();
    }

private:
    bool compute_ = true;
    bool initialized_ = false;
    int current_ = 0;
    gfx::Buffer particles_;
    gfx::Texture pos_[2], vel_[2], hdr_;
    gfx::Pipeline simPipeline_, drawPipeline_, compositePipeline_;
};

}  // namespace

std::unique_ptr<Scene> makeParticlesScene() { return std::make_unique<ParticlesScene>(); }
