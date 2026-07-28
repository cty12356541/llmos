/* NLOS visual effects v6
   Three.js owns the ambient background.
   GSAP + ScrollTrigger exclusively own content entrance animation.
   Pointer effects are progressive enhancements and never gate content. */
(function () {
  "use strict";

  /*
   * Use the classic local build so this file can still install the v6 CSS
   * fallback if Three.js is delayed or unavailable. A failed top-level module
   * import used to prevent every recovery path in this file from running.
   */
  var THREE = window.THREE;

  var REDUCE_QUERY = window.matchMedia(
    "(prefers-reduced-motion: reduce)"
  );
  var FINE_POINTER_QUERY = window.matchMedia("(pointer: fine)");
  var DESKTOP_QUERY = window.matchMedia(
    "(min-width: 900px) and (pointer: fine)"
  );
  var MOBILE_QUERY = window.matchMedia("(max-width: 720px)");

  var REDUCE = REDUCE_QUERY.matches;
  var FINE_POINTER = FINE_POINTER_QUERY.matches;

  var diagnostics = {
    version: "v6-aurora",
    background: "pending",
    targetFps: MOBILE_QUERY.matches ? 30 : 60,
    qualityTier: MOBILE_QUERY.matches ? "mobile" : "high",
    averageFrameMs: 0,
    particleCount: 0
  };

  window.NLOSEffects = {
    getStats: function () {
      return Object.assign({}, diagnostics);
    }
  };
  document.documentElement.dataset.effectsVersion = diagnostics.version;
  document.documentElement.dataset.effectsTargetFps = String(
    diagnostics.targetFps
  );

  function enableCssFallback(reason) {
    diagnostics.background = reason || "css-fallback";
    document.documentElement.dataset.effectsBackground =
      diagnostics.background;
    document.body.classList.add("css-aurora");
    var canvas = document.getElementById("bg-canvas");
    if (canvas) canvas.hidden = true;
  }

  /* ============================================================
     Three.js aurora background
     One renderer, one frame loop, adaptive desktop quality.
     ============================================================ */
  function initAuroraBackground() {
    var canvas = document.getElementById("bg-canvas");
    if (!canvas) {
      diagnostics.background = "missing-canvas";
      return;
    }
    if (!THREE) {
      enableCssFallback("three-unavailable");
      return;
    }
    if (REDUCE) {
      enableCssFallback("reduced-motion");
      return;
    }

    var isMobile = MOBILE_QUERY.matches;
    var targetFps = isMobile ? 30 : 60;
    var frameInterval = 1000 / targetFps;
    var qualityStep = isMobile ? 1 : 0;
    var maxDpr = isMobile ? 1.25 : 1.5;
    var particleCount = isMobile ? 28 : 58;
    var visibleParticleCount = particleCount;
    var renderer;
    var contextAttributes = {
      alpha: false,
      antialias: false,
      powerPreference: isMobile ? "default" : "high-performance"
    };
    var gl =
      canvas.getContext("webgl2", contextAttributes) ||
      canvas.getContext("webgl", contextAttributes);
    if (!gl) {
      enableCssFallback("webgl-unavailable");
      return;
    }

    try {
      renderer = new THREE.WebGLRenderer({
        canvas: canvas,
        context: gl,
        antialias: false,
        alpha: false,
        powerPreference: isMobile ? "default" : "high-performance"
      });
    } catch (error) {
      enableCssFallback("webgl-unavailable");
      return;
    }

    diagnostics.background = "three";
    document.documentElement.dataset.effectsBackground = "three";
    document.documentElement.dataset.effectsQuality =
      diagnostics.qualityTier;
    diagnostics.targetFps = targetFps;
    diagnostics.particleCount = particleCount;
    document.body.classList.add("webgl-ready");

    renderer.outputColorSpace = THREE.SRGBColorSpace;

    var scene = new THREE.Scene();
    var camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.01, 4);
    camera.position.z = 2;

    var uniforms = {
      uTime: { value: 0 },
      uPointer: { value: new THREE.Vector2(0.5, 0.5) },
      uResolution: { value: new THREE.Vector2(1, 1) },
      uDetail: { value: 1 }
    };

    var vertexShader = [
      "varying vec2 vUv;",
      "void main() {",
      "  vUv = uv;",
      "  gl_Position = vec4(position, 1.0);",
      "}"
    ].join("\n");

    var fragmentShader = [
      "precision highp float;",
      "uniform float uTime;",
      "uniform vec2 uPointer;",
      "uniform vec2 uResolution;",
      "uniform float uDetail;",
      "varying vec2 vUv;",
      "float ribbon(float y, float center, float width) {",
      "  float d = abs(y - center) / width;",
      "  return exp(-d * d * 2.35);",
      "}",
      "void main() {",
      "  vec2 uv = vUv;",
      "  vec2 p = uv * 2.0 - 1.0;",
      "  p.x *= uResolution.x / max(uResolution.y, 1.0);",
      "  float t = uTime * 0.16;",
      "  float pointerBend = (uPointer.x - 0.5) * 0.045;",
      "  float x = uv.x;",
      "",
      "  float waveA = 0.73 + sin(x * 5.2 + t) * 0.070",
      "    + sin(x * 11.0 - t * 0.72) * 0.018 + pointerBend;",
      "  float waveB = 0.48 + sin(x * 4.0 - t * 0.64 + 1.8) * 0.085",
      "    + sin(x * 8.4 + t * 0.46) * 0.024 - pointerBend * 0.6;",
      "  float waveC = 0.22 + sin(x * 3.2 + t * 0.38 + 4.1) * 0.060",
      "    + sin(x * 9.0 - t * 0.31) * 0.016;",
      "",
      "  float auroraA = ribbon(uv.y, waveA, 0.052);",
      "  float auroraB = ribbon(uv.y, waveB, 0.074);",
      "  float auroraC = ribbon(uv.y, waveC, 0.050);",
      "  float coreA = ribbon(uv.y, waveA, 0.012);",
      "  float coreB = ribbon(uv.y, waveB, 0.017);",
      "  float coreC = ribbon(uv.y, waveC, 0.010);",
      "  float filament = 0.58 + 0.42 * sin(x * 42.0 + t * 1.9",
      "    + sin(x * 8.0) * 1.8);",
      "  filament = mix(0.72, filament, uDetail);",
      "  float edgeFade = smoothstep(0.0, 0.12, x) * smoothstep(1.0, 0.88, x);",
      "",
      "  vec3 deep = vec3(0.012, 0.037, 0.070);",
      "  vec3 blue = vec3(0.035, 0.27, 0.52);",
      "  vec3 cyan = vec3(0.22, 0.72, 1.0);",
      "  vec3 ice = vec3(0.72, 0.94, 1.0);",
      "  vec3 color = deep;",
      "  color += blue * auroraC * 0.095 * edgeFade;",
      "  color += cyan * auroraB * (0.105 + filament * 0.045) * edgeFade;",
      "  color += ice * auroraA * (0.075 + filament * 0.055) * edgeFade;",
      "  color += cyan * coreC * 0.12 * edgeFade;",
      "  color += ice * coreB * 0.11 * edgeFade;",
      "  color += ice * coreA * (0.14 + filament * 0.08) * edgeFade;",
      "",
      "  float curtainA = smoothstep(waveA - 0.24, waveA, uv.y)",
      "    * (1.0 - smoothstep(waveA - 0.10, waveA + 0.03, uv.y));",
      "  float curtainB = smoothstep(waveB - 0.18, waveB, uv.y)",
      "    * (1.0 - smoothstep(waveB - 0.08, waveB + 0.02, uv.y));",
      "  color += cyan * curtainA * filament * 0.018 * edgeFade;",
      "  color += blue * curtainB * (1.0 - filament * 0.4) * 0.016 * edgeFade;",
      "",
      "  float horizon = smoothstep(0.0, 1.0, uv.y) * 0.012;",
      "  color += vec3(0.12, 0.38, 0.62) * horizon;",
      "  float vignette = smoothstep(1.34, 0.20, length(p * vec2(0.58, 0.78)));",
      "  color *= mix(0.80, 1.04, vignette);",
      "  gl_FragColor = vec4(color, 1.0);",
      "}"
    ].join("\n");

    var plane = new THREE.Mesh(
      new THREE.PlaneGeometry(2, 2),
      new THREE.ShaderMaterial({
        uniforms: uniforms,
        vertexShader: vertexShader,
        fragmentShader: fragmentShader,
        depthTest: false,
        depthWrite: false
      })
    );
    plane.renderOrder = 0;
    scene.add(plane);

    /* Deterministic star field prevents layout flashes between page loads. */
    var seed = 9137;
    function random() {
      seed = (seed * 16807) % 2147483647;
      return (seed - 1) / 2147483646;
    }

    var pointPositions = new Float32Array(particleCount * 3);
    var pointVelocity = new Float32Array(particleCount * 2);
    var pointPhase = new Float32Array(particleCount);

    for (var i = 0; i < particleCount; i += 1) {
      pointPositions[i * 3] = random() * 2 - 1;
      pointPositions[i * 3 + 1] = random() * 2 - 1;
      pointPositions[i * 3 + 2] = 0.12;
      pointVelocity[i * 2] = (random() - 0.5) * 0.006;
      pointVelocity[i * 2 + 1] = 0.004 + random() * 0.008;
      pointPhase[i] = random() * Math.PI * 2;
    }

    var pointGeometry = new THREE.BufferGeometry();
    var pointAttribute = new THREE.BufferAttribute(pointPositions, 3);
    pointGeometry.setAttribute("position", pointAttribute);
    pointGeometry.setDrawRange(0, visibleParticleCount);

    var points = new THREE.Points(
      pointGeometry,
      new THREE.PointsMaterial({
        color: 0x9fddff,
        size: isMobile ? 1.35 : 1.75,
        sizeAttenuation: false,
        transparent: true,
        opacity: isMobile ? 0.42 : 0.58,
        blending: THREE.AdditiveBlending,
        depthTest: false,
        depthWrite: false
      })
    );
    points.renderOrder = 2;
    scene.add(points);

    var maxLinks = isMobile ? 8 : 20;
    var linkPairs = [];
    for (var linkIndex = 0; linkIndex < particleCount; linkIndex += 2) {
      if (linkPairs.length >= maxLinks) break;
      var nearestIndex = -1;
      var nearestDistance = 0.34 * 0.34;
      for (
        var candidate = linkIndex + 1;
        candidate < particleCount;
        candidate += 1
      ) {
        var dx =
          pointPositions[linkIndex * 3] - pointPositions[candidate * 3];
        var dy =
          pointPositions[linkIndex * 3 + 1] -
          pointPositions[candidate * 3 + 1];
        var distance = dx * dx + dy * dy;
        if (distance < nearestDistance) {
          nearestDistance = distance;
          nearestIndex = candidate;
        }
      }
      if (nearestIndex >= 0) linkPairs.push([linkIndex, nearestIndex]);
    }

    var linePositions = new Float32Array(linkPairs.length * 6);
    var lineGeometry = new THREE.BufferGeometry();
    var lineAttribute = new THREE.BufferAttribute(linePositions, 3);
    lineGeometry.setAttribute("position", lineAttribute);

    var lines = new THREE.LineSegments(
      lineGeometry,
      new THREE.LineBasicMaterial({
        color: 0x76c9f8,
        transparent: true,
        opacity: isMobile ? 0.035 : 0.065,
        blending: THREE.AdditiveBlending,
        depthTest: false,
        depthWrite: false
      })
    );
    lines.renderOrder = 1;
    scene.add(lines);

    var pointerTarget = { x: 0.5, y: 0.5 };
    window.addEventListener(
      "pointermove",
      function (event) {
        if (!FINE_POINTER) return;
        pointerTarget.x = event.clientX / Math.max(window.innerWidth, 1);
        pointerTarget.y = 1 - event.clientY / Math.max(window.innerHeight, 1);
      },
      { passive: true }
    );

    function resize() {
      var dpr = Math.min(window.devicePixelRatio || 1, maxDpr);
      renderer.setPixelRatio(dpr);
      renderer.setSize(window.innerWidth, window.innerHeight, false);
      uniforms.uResolution.value.set(
        window.innerWidth * dpr,
        window.innerHeight * dpr
      );
    }

    var resizeQueued = false;
    window.addEventListener(
      "resize",
      function () {
        if (resizeQueued) return;
        resizeQueued = true;
        window.requestAnimationFrame(function () {
          resize();
          resizeQueued = false;
        });
      },
      { passive: true }
    );
    resize();

    function updateParticles(deltaSeconds, nowSeconds) {
      for (var index = 0; index < visibleParticleCount; index += 1) {
        var offset = index * 3;
        pointPositions[offset] +=
          pointVelocity[index * 2] * deltaSeconds +
          Math.sin(nowSeconds * 0.16 + pointPhase[index]) * 0.000018;
        pointPositions[offset + 1] +=
          pointVelocity[index * 2 + 1] * deltaSeconds;

        if (pointPositions[offset] > 1.06) pointPositions[offset] = -1.06;
        if (pointPositions[offset] < -1.06) pointPositions[offset] = 1.06;
        if (pointPositions[offset + 1] > 1.06) {
          pointPositions[offset + 1] = -1.06;
        }
      }
      pointAttribute.needsUpdate = true;

      for (var link = 0; link < linkPairs.length; link += 1) {
        var a = linkPairs[link][0];
        var b = linkPairs[link][1];
        var lineOffset = link * 6;
        linePositions[lineOffset] = pointPositions[a * 3];
        linePositions[lineOffset + 1] = pointPositions[a * 3 + 1];
        linePositions[lineOffset + 2] = 0.1;
        linePositions[lineOffset + 3] = pointPositions[b * 3];
        linePositions[lineOffset + 4] = pointPositions[b * 3 + 1];
        linePositions[lineOffset + 5] = 0.1;
      }
      lineAttribute.needsUpdate = true;
    }

    function applyQualityStep(step) {
      qualityStep = step;
      if (step === 1) {
        maxDpr = Math.min(maxDpr, 1.25);
        visibleParticleCount = Math.min(
          visibleParticleCount,
          isMobile ? 24 : 40
        );
        pointGeometry.setDrawRange(0, visibleParticleCount);
        lines.material.opacity *= 0.72;
        diagnostics.qualityTier = isMobile ? "mobile" : "balanced";
        document.documentElement.dataset.effectsQuality =
          diagnostics.qualityTier;
        resize();
      } else if (step >= 2) {
        uniforms.uDetail.value = 0;
        visibleParticleCount = Math.min(visibleParticleCount, 30);
        pointGeometry.setDrawRange(0, visibleParticleCount);
        diagnostics.qualityTier = "performance";
        document.documentElement.dataset.effectsQuality =
          diagnostics.qualityTier;
      }
      diagnostics.particleCount = visibleParticleCount;
    }

    var running = true;
    var bgLive = false;
    var lastRendered = performance.now();
    var frameSamples = [];
    var sampleCooldown = 0;
    var renderedFrames = 0;
    var fpsWindowStart = lastRendered;

    function render(now) {
      if (!running) return;
      window.requestAnimationFrame(render);
      if (document.hidden) {
        lastRendered = now;
        return;
      }

      var elapsed = now - lastRendered;
      if (elapsed < frameInterval - 0.6) return;
      lastRendered = now - (elapsed % frameInterval);

      var safeElapsed = Math.min(elapsed, 64);
      var deltaSeconds = safeElapsed / 1000;
      uniforms.uTime.value += deltaSeconds;
      uniforms.uPointer.value.x +=
        (pointerTarget.x - uniforms.uPointer.value.x) * 0.025;
      uniforms.uPointer.value.y +=
        (pointerTarget.y - uniforms.uPointer.value.y) * 0.025;

      updateParticles(deltaSeconds, now / 1000);
      renderer.render(scene, camera);
      renderedFrames += 1;

      if (!bgLive) {
        bgLive = true;
        document.body.classList.add("bg-live");
      }

      if (now - fpsWindowStart >= 2000) {
        var actualFps =
          (renderedFrames * 1000) / Math.max(now - fpsWindowStart, 1);
        document.documentElement.dataset.effectsFps =
          actualFps.toFixed(1);
        renderedFrames = 0;
        fpsWindowStart = now;
      }

      if (!isMobile && sampleCooldown <= 0) {
        frameSamples.push(safeElapsed);
        if (frameSamples.length >= 120) {
          var average =
            frameSamples.reduce(function (sum, value) {
              return sum + value;
            }, 0) / frameSamples.length;
          diagnostics.averageFrameMs = Number(average.toFixed(2));
          document.documentElement.dataset.effectsFrameMs =
            average.toFixed(2);
          frameSamples.length = 0;

          if (average > 24 && qualityStep === 1) {
            applyQualityStep(2);
            sampleCooldown = 180;
          } else if (average > 20 && qualityStep === 0) {
            applyQualityStep(1);
            sampleCooldown = 180;
          }
        }
      } else {
        sampleCooldown -= 1;
      }
    }

    canvas.addEventListener("webglcontextlost", function (event) {
      event.preventDefault();
      running = false;
      document.body.classList.remove("webgl-ready");
      enableCssFallback("context-lost");
    });

    diagnostics.particleCount = visibleParticleCount;
    window.requestAnimationFrame(render);
  }

  /* ============================================================
     Precise custom cursor — desktop fine-pointer only.
     ============================================================ */
  function initCustomCursor() {
    if (REDUCE || !FINE_POINTER || !DESKTOP_QUERY.matches) return;

    var dot = document.createElement("span");
    var ring = document.createElement("span");
    dot.className = "cursor-dot";
    ring.className = "cursor-ring";
    dot.setAttribute("aria-hidden", "true");
    ring.setAttribute("aria-hidden", "true");
    document.body.appendChild(dot);
    document.body.appendChild(ring);

    var targetX = window.innerWidth / 2;
    var targetY = window.innerHeight / 2;
    var ringX = targetX;
    var ringY = targetY;
    var cursorRunning = false;
    var lastMove = 0;

    function frame(now) {
      var dx = targetX - ringX;
      var dy = targetY - ringY;
      ringX += dx * 0.28;
      ringY += dy * 0.28;

      dot.style.transform =
        "translate3d(" + targetX + "px," + targetY + "px,0)";
      ring.style.transform =
        "translate3d(" + ringX + "px," + ringY + "px,0)";

      if (
        Math.abs(dx) > 0.08 ||
        Math.abs(dy) > 0.08 ||
        now - lastMove < 220
      ) {
        window.requestAnimationFrame(frame);
      } else {
        cursorRunning = false;
      }
    }

    function startCursorLoop() {
      if (cursorRunning) return;
      cursorRunning = true;
      window.requestAnimationFrame(frame);
    }

    window.addEventListener(
      "pointermove",
      function (event) {
        targetX = event.clientX;
        targetY = event.clientY;
        lastMove = performance.now();

        var nativeZone = event.target.closest(
          "input, textarea, select, [contenteditable='true'], pre, code"
        );
        var hoverZone = event.target.closest(
          "a, button, .btn, .nav-card, [data-cursor]"
        );
        document.body.classList.toggle(
          "custom-cursor-native",
          Boolean(nativeZone)
        );
        document.body.classList.toggle(
          "custom-cursor-hover",
          Boolean(hoverZone && !nativeZone)
        );
        document.body.classList.add("custom-cursor-ready");
        startCursorLoop();
      },
      { passive: true }
    );

    window.addEventListener("pointerdown", function () {
      document.body.classList.add("custom-cursor-down");
    });
    window.addEventListener("pointerup", function () {
      document.body.classList.remove("custom-cursor-down");
    });
    document.documentElement.addEventListener("pointerleave", function () {
      document.body.classList.remove(
        "custom-cursor-ready",
        "custom-cursor-hover",
        "custom-cursor-down"
      );
    });
    document.addEventListener("visibilitychange", function () {
      if (document.hidden) {
        document.body.classList.remove("custom-cursor-ready");
      }
    });
  }

  /* ============================================================
     Restrained 3D card tilt.
     ============================================================ */
  function initCardTilt() {
    if (REDUCE || !DESKTOP_QUERY.matches) return;

    var cards = Array.prototype.slice.call(
      document.querySelectorAll(
        ".nav-card, .autopsy-card, .abs-card, .stat, .principle"
      )
    );

    function motionSettled(card) {
      var group = card.closest(".reveal-group");
      return (
        (!card.classList.contains("reveal") ||
          card.classList.contains("in")) &&
        (!group || group.classList.contains("in"))
      );
    }

    function resetCard(card) {
      if (!card.classList.contains("is-tilting")) return;
      card.classList.remove("is-tilting");
      card.style.removeProperty("transform");
      card.style.removeProperty("--sheen-x");
      card.style.removeProperty("--sheen-y");
    }

    cards.forEach(function (card) {
      card.setAttribute("data-tilt", "");
      var sheen = card.querySelector(".sheen");
      if (!sheen) {
        sheen = document.createElement("span");
        sheen.className = "sheen";
        sheen.setAttribute("aria-hidden", "true");
        card.appendChild(sheen);
      }

      var tiltQueued = false;
      var pointerX = 0.5;
      var pointerY = 0.5;

      card.addEventListener(
        "pointermove",
        function (event) {
          if (!DESKTOP_QUERY.matches || !motionSettled(card)) {
            resetCard(card);
            return;
          }
          var rect = card.getBoundingClientRect();
          pointerX = (event.clientX - rect.left) / rect.width;
          pointerY = (event.clientY - rect.top) / rect.height;
          if (tiltQueued) return;
          tiltQueued = true;

          window.requestAnimationFrame(function () {
            var rotateX = (0.5 - pointerY) * 5;
            var rotateY = (pointerX - 0.5) * 5;
            card.style.transform =
              "perspective(1100px) rotateX(" +
              rotateX.toFixed(2) +
              "deg) rotateY(" +
              rotateY.toFixed(2) +
              "deg) translateY(-2px)";
            card.style.setProperty("--sheen-x", pointerX * 100 + "%");
            card.style.setProperty("--sheen-y", pointerY * 100 + "%");
            card.classList.add("is-tilting");
            tiltQueued = false;
          });
        },
        { passive: true }
      );

      card.addEventListener("pointerleave", function () {
        resetCard(card);
      });
      card.addEventListener("pointercancel", function () {
        resetCard(card);
      });
      card.addEventListener("focusout", function () {
        resetCard(card);
      });
    });

    window.addEventListener("blur", function () {
      cards.forEach(resetCard);
    });
  }

  /* ============================================================
     GSAP motion — the only content entrance system.
     Content remains visible if GSAP or ScrollTrigger is absent.
     ============================================================ */
  function initScrollMotion() {
    var gsap = window.gsap;
    var ScrollTrigger = window.ScrollTrigger;
    var root = document.documentElement;

    function clearRevealFailsafe() {
      if (!window.__nlosRevealFailsafe) return;
      clearTimeout(window.__nlosRevealFailsafe);
      window.__nlosRevealFailsafe = null;
    }

    /*
     * If the timeout already exposed the page, do not hide it again and cause
     * a late-loading flash. Otherwise keep the timeout armed until the whole
     * motion graph has been registered successfully.
     */
    if (root.classList.contains("reveal-all")) {
      diagnostics.motion = "fallback-timeout";
      clearRevealFailsafe();
      return;
    }

    if (REDUCE || !gsap || !ScrollTrigger) {
      diagnostics.motion = REDUCE ? "reduced" : "unavailable";
      clearRevealFailsafe();
      root.classList.add("reveal-all");
      return;
    }

    diagnostics.motion = "gsap";
    gsap.registerPlugin(ScrollTrigger);

    function clearMotionProperties(targets) {
      gsap.set(targets, {
        clearProps: "opacity,visibility,transform,filter"
      });
    }

    function persistVisible(element) {
      if (!element || !element.classList) return;
      if (
        element.classList.contains("reveal") ||
        element.classList.contains("reveal-group")
      ) {
        element.classList.add("in");
      }
    }

    function settleTargets(targets) {
      targets.forEach(function (element) {
        persistVisible(element);
        persistVisible(element.parentElement);
      });
      clearMotionProperties(targets);
    }

    function settleParentsInstant(targets) {
      targets.forEach(function (element) {
        var parent = element.parentElement;
        if (
          parent &&
          parent.classList &&
          parent.classList.contains("reveal") &&
          !parent.classList.contains("in")
        ) {
          parent.classList.add("in");
        }
      });
    }

    function animateTargets(targets, stagger, distance) {
      if (!targets || !targets.length) return;
      gsap.fromTo(
        targets,
        {
          autoAlpha: 0,
          y: distance || 14,
          filter: "blur(3px)"
        },
        {
          autoAlpha: 1,
          y: 0,
          filter: "blur(0px)",
          duration: 0.62,
          stagger: stagger || 0.055,
          ease: "power2.out",
          overwrite: "auto",
          onComplete: function () {
            settleTargets(targets);
          }
        }
      );
    }

    var pageLead = document.querySelector(".hero, .page-head");
    if (pageLead) {
      var leadTargets = Array.prototype.slice.call(
        pageLead.querySelectorAll(
          ".eyebrow, .title-line, .hero-desc, .page-desc, " +
            ".hero-thesis, .hero-cta, .hero-meta, .page-toc, .flow"
        )
      );
      settleParentsInstant(leadTargets);
      animateTargets(leadTargets, 0.065, 12);
    }

    function prepareReveal(targets, trigger, stagger) {
      if (!targets.length) return;
      var rect = trigger.getBoundingClientRect();
      if (rect.bottom < 0) {
        settleTargets(targets);
        return;
      }
      if (rect.top < window.innerHeight * 0.9) {
        settleParentsInstant(targets);
        animateTargets(targets, stagger, 16);
        return;
      }

      ScrollTrigger.create({
        trigger: trigger,
        start: "top 88%",
        once: true,
        onEnter: function (self) {
          settleParentsInstant(targets);
          animateTargets(targets, stagger, 18);
          self.kill();
        }
      });
    }

    Array.prototype.slice
      .call(document.querySelectorAll(".reveal-group"))
      .forEach(function (group) {
        prepareReveal(
          Array.prototype.slice.call(group.children),
          group,
          0.06
        );
      });

    Array.prototype.slice
      .call(document.querySelectorAll(".reveal"))
      .filter(function (element) {
        return !element.closest(".hero, .page-head, .reveal-group");
      })
      .forEach(function (element) {
        prepareReveal([element], element, 0);
      });

    ScrollTrigger.refresh();
    clearRevealFailsafe();
  }

  function init() {
    initAuroraBackground();
    initCustomCursor();
    initCardTilt();
    initScrollMotion();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init, { once: true });
  } else {
    init();
  }
})();
