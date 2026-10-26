/* ==========================================================================
   NLOS Documentation & Product Site — Shader, Spotlight & Motion Engine v11.0
   Local vendor runtime: THREE (r128+), CSS transitions + IntersectionObserver.
   Bulletproof progressive fallbacks: zero stuck hidden elements, zero CDN.
   ========================================================================== */
(function () {
  'use strict';

  var docEl = document.documentElement;
  var reduceMotion =
    window.matchMedia &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  var finePointer =
    window.matchMedia &&
    window.matchMedia('(hover: hover) and (pointer: fine)').matches;

  function clearRevealFailsafe() {
    if (window.__nlosRevealFailsafe) {
      clearTimeout(window.__nlosRevealFailsafe);
      window.__nlosRevealFailsafe = null;
    }
  }

  function revealAllImmediately() {
    clearRevealFailsafe();
    docEl.classList.add('reveal-all');
    document
      .querySelectorAll('.reveal, .reveal-group')
      .forEach(function (el) {
        el.classList.add('in');
      });
  }

  /* ------------------------------------------------------------------------
     1. Obsidian-Glacier WebGL Aurora Shader (#bg-canvas)
     ------------------------------------------------------------------------ */
  function initAuroraShader() {
    var canvas = document.getElementById('bg-canvas');
    if (!canvas) return;

    if (reduceMotion || !window.THREE) {
      docEl.classList.add('no-webgl');
      if (document.body) document.body.classList.add('css-aurora');
      return;
    }

    var renderer;
    try {
      renderer = new window.THREE.WebGLRenderer({
        canvas: canvas,
        alpha: false,
        antialias: false,
        powerPreference: 'low-power',
        stencil: false,
        depth: false
      });
    } catch (err) {
      docEl.classList.add('no-webgl');
      document.body.classList.add('css-aurora');
      return;
    }

    var THREE = window.THREE;
    var scene = new THREE.Scene();
    var camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0, 1);

    var uniforms = {
      uTime: { value: 0 },
      uResolution: {
        value: new THREE.Vector2(window.innerWidth, window.innerHeight)
      },
      uPointer: { value: new THREE.Vector2(0.5, 0.5) }
    };

    var vertexShader = [
      'varying vec2 vUv;',
      'void main() {',
      '  vUv = uv;',
      '  gl_Position = vec4(position.xy, 0.0, 1.0);',
      '}'
    ].join('\n');

    var fragmentShader = [
      'precision mediump float;',
      'varying vec2 vUv;',
      'uniform float uTime;',
      'uniform vec2 uResolution;',
      'uniform vec2 uPointer;',
      '',
      'float hash(vec2 p) {',
      '  return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453123);',
      '}',
      '',
      'float noise(vec2 p) {',
      '  vec2 i = floor(p);',
      '  vec2 f = fract(p);',
      '  vec2 u = f * f * (3.0 - 2.0 * f);',
      '  return mix(',
      '    mix(hash(i + vec2(0.0, 0.0)), hash(i + vec2(1.0, 0.0)), u.x),',
      '    mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x),',
      '    u.y',
      '  );',
      '}',
      '',
      'float fbm(vec2 p) {',
      '  float v = 0.0;',
      '  float a = 0.5;',
      '  mat2 rot = mat2(0.80, 0.60, -0.60, 0.80);',
      '  for (int i = 0; i < 4; i++) {',
      '    v += a * noise(p);',
      '    p = rot * p * 2.02;',
      '    a *= 0.5;',
      '  }',
      '  return v;',
      '}',
      '',
      'void main() {',
      '  vec2 uv = vUv;',
      '  float aspect = uResolution.x / max(1.0, uResolution.y);',
      '  vec2 p = vec2((uv.x - 0.5) * aspect, uv.y - 0.5);',
      '  float t = uTime * 0.055;',
      '',
      '  vec2 ptr = (uPointer - 0.5) * vec2(0.08, 0.05);',
      '  float warp = fbm(vec2(p.x * 1.35 + t * 0.7, p.y * 2.1 - t * 0.35));',
      '',
      '  // Obsidian base (#040a14)',
      '  vec3 col = vec3(0.0157, 0.0392, 0.0784);',
      '',
      '  // Upper glacier horizon ribbon (~76% UV Y => 24% from top)',
      '  float y1 = 0.76 + 0.045 * sin(p.x * 1.9 + t * 1.3 + warp * 1.8) + ptr.y;',
      '  float r1 = exp(-pow((uv.y - y1) / 0.115, 2.0));',
      '  r1 *= 0.55 + 0.45 * fbm(vec2(p.x * 2.4 - t, uv.y * 3.0));',
      '',
      '  // Mid architectural cyan stream (~48% UV Y)',
      '  float y2 = 0.48 + 0.055 * cos(p.x * 1.45 - t * 0.95 + warp * 1.5) - ptr.y * 0.6;',
      '  float r2 = exp(-pow((uv.y - y2) / 0.145, 2.0));',
      '  r2 *= 0.50 + 0.50 * warp;',
      '',
      '  // Deep abyssal undertone (~21% UV Y)',
      '  float y3 = 0.21 + 0.035 * sin(p.x * 2.1 + t * 0.75);',
      '  float r3 = exp(-pow((uv.y - y3) / 0.125, 2.0));',
      '',
      '  vec3 iceGlow = vec3(0.66, 0.91, 1.00);',
      '  vec3 cyanMid = vec3(0.28, 0.60, 0.86);',
      '  vec3 deepBlue = vec3(0.10, 0.38, 0.66);',
      '',
      '  col += iceGlow * r1 * 0.125;',
      '  col += cyanMid * r2 * 0.130;',
      '  col += deepBlue * r3 * 0.105;',
      '',
      '  // Subtle pointer atmospheric halo',
      '  vec2 ptrUv = vec2((uPointer.x - 0.5) * aspect, uPointer.y - 0.5);',
      '  float ptrDist = length(p - ptrUv);',
      '  col += vec3(0.42, 0.78, 0.98) * exp(-ptrDist * 3.6) * 0.032;',
      '',
      '  // Edge vignette to frame editorial content cleanly',
      '  float vig = smoothstep(1.22, 0.28, length(vec2(p.x * 0.72, p.y * 0.95)));',
      '  col *= mix(0.74, 1.0, vig);',
      '',
      '  // Anti-banding subpixel dither',
      '  col += (hash(gl_FragCoord.xy + fract(t)) - 0.5) / 255.0;',
      '',
      '  gl_FragColor = vec4(col, 1.0);',
      '}'
    ].join('\n');

    var geometry = new THREE.PlaneGeometry(2, 2);
    var material = new THREE.ShaderMaterial({
      uniforms: uniforms,
      vertexShader: vertexShader,
      fragmentShader: fragmentShader,
      depthWrite: false,
      depthTest: false
    });
    var mesh = new THREE.Mesh(geometry, material);
    scene.add(mesh);

    var targetPtrX = 0.5;
    var targetPtrY = 0.5;
    var curPtrX = 0.5;
    var curPtrY = 0.5;
    var rafId = 0;
    var contextLost = false;
    var startTime = performance.now();

    function syncSize() {
      var w = Math.max(1, window.innerWidth);
      var h = Math.max(1, window.innerHeight);
      var dpr = Math.min(window.devicePixelRatio || 1, 1.75);
      renderer.setPixelRatio(dpr);
      renderer.setSize(w, h, false);
      uniforms.uResolution.value.set(w, h);
    }

    function render(now) {
      rafId = 0;
      if (contextLost || document.hidden) return;

      curPtrX += (targetPtrX - curPtrX) * 0.045;
      curPtrY += (targetPtrY - curPtrY) * 0.045;
      uniforms.uPointer.value.set(curPtrX, curPtrY);
      uniforms.uTime.value = (now - startTime) * 0.001;

      renderer.render(scene, camera);

      if (!document.body.classList.contains('bg-live')) {
        document.body.classList.add('bg-live');
      }

      rafId = requestAnimationFrame(render);
    }

    function start() {
      if (!rafId && !contextLost && !document.hidden) {
        rafId = requestAnimationFrame(render);
      }
    }

    syncSize();
    start();

    window.addEventListener('resize', syncSize, { passive: true });

    if (finePointer) {
      window.addEventListener(
        'pointermove',
        function (e) {
          targetPtrX = e.clientX / Math.max(1, window.innerWidth);
          targetPtrY = 1.0 - e.clientY / Math.max(1, window.innerHeight);
        },
        { passive: true }
      );
    }

    document.addEventListener('visibilitychange', function () {
      if (document.hidden) {
        if (rafId) {
          cancelAnimationFrame(rafId);
          rafId = 0;
        }
      } else {
        start();
      }
    });

    canvas.addEventListener(
      'webglcontextlost',
      function (event) {
        event.preventDefault();
        contextLost = true;
        if (rafId) {
          cancelAnimationFrame(rafId);
          rafId = 0;
        }
        document.body.classList.remove('bg-live');
        document.body.classList.add('css-aurora');
        docEl.classList.add('no-webgl');
      },
      false
    );
  }

  /* ------------------------------------------------------------------------
     2. Restrained Glass Spotlight & Micro-Tilt ([data-tilt])
     ------------------------------------------------------------------------ */
  function initCardTilt() {
    if (reduceMotion || !finePointer) return;

    var TILT_SELECTOR = [
      '.nav-card',
      '.autopsy-card',
      '.arch-layer',
      '.abs-card',
      '.tier',
      '.bento-card',
      '.metric-card',
      '.stage-card',
      '.wp-item',
      '.entry-card'
    ].join(', ');

    var MAX_TILT = 2.2;
    var cards = document.querySelectorAll(TILT_SELECTOR);

    cards.forEach(function (card) {
      if (
        card.classList.contains('metric-card') &&
        card.parentElement &&
        card.parentElement.classList.contains('metric-strip')
      ) {
        return;
      }

      card.setAttribute('data-tilt', '');
      if (!card.querySelector(':scope > .sheen')) {
        var sheen = document.createElement('div');
        sheen.className = 'sheen';
        sheen.setAttribute('aria-hidden', 'true');
        card.insertBefore(sheen, card.firstChild);
      }

      var cardRaf = 0;
      var lastX = 0;
      var lastY = 0;

      function applyTilt() {
        cardRaf = 0;
        var rect = card.getBoundingClientRect();
        if (!rect.width || !rect.height) return;

        var px = (lastX - rect.left) / rect.width;
        var py = (lastY - rect.top) / rect.height;
        var clampedX = Math.max(0, Math.min(1, px));
        var clampedY = Math.max(0, Math.min(1, py));

        var rotY = (clampedX - 0.5) * (MAX_TILT * 2);
        var rotX = (0.5 - clampedY) * (MAX_TILT * 2);

        card.style.setProperty('--sheen-x', (clampedX * 100).toFixed(1) + '%');
        card.style.setProperty('--sheen-y', (clampedY * 100).toFixed(1) + '%');
        card.style.transform =
          'perspective(960px) rotateX(' +
          rotX.toFixed(2) +
          'deg) rotateY(' +
          rotY.toFixed(2) +
          'deg) translateY(-2px)';
      }

      card.addEventListener(
        'pointerenter',
        function () {
          card.classList.add('is-tilting');
        },
        { passive: true }
      );

      card.addEventListener(
        'pointermove',
        function (e) {
          lastX = e.clientX;
          lastY = e.clientY;
          if (!cardRaf) cardRaf = requestAnimationFrame(applyTilt);
        },
        { passive: true }
      );

      card.addEventListener(
        'pointerleave',
        function () {
          if (cardRaf) {
            cancelAnimationFrame(cardRaf);
            cardRaf = 0;
          }
          card.classList.remove('is-tilting');
          card.style.transform = '';
        },
        { passive: true }
      );
    });
  }

  /* ------------------------------------------------------------------------
     3. Legacy Hero SVG Flow Highlighting (.flow)
     ------------------------------------------------------------------------ */
  function initFlowHighlight() {
    var flows = document.querySelectorAll('.flow svg');
    flows.forEach(function (svg) {
      var curves = Array.prototype.slice.call(
        svg.querySelectorAll('.flow-curve')
      );
      var pills = Array.prototype.slice.call(
        svg.querySelectorAll('g[data-flow], .flow-node')
      );
      if (!curves.length || !pills.length) return;

      pills.forEach(function (pill, index) {
        pill.addEventListener('mouseenter', function () {
          curves.forEach(function (c, i) {
            c.classList.toggle('is-highlighted', i === index);
            c.classList.toggle('is-muted', i !== index);
          });
        });
        pill.addEventListener('mouseleave', function () {
          curves.forEach(function (c) {
            c.classList.remove('is-highlighted', 'is-muted');
          });
        });
      });
    });
  }

  /* ------------------------------------------------------------------------
     4. Deterministic Scroll Reveal Engine (CSS Transition + Sweep + IO)
     ------------------------------------------------------------------------ */
  function initScrollMotion() {
    if (reduceMotion) {
      revealAllImmediately();
      return;
    }

    try {
      var groups = document.querySelectorAll('.reveal-group');
      for (var g = 0; g < groups.length; g++) {
        var children = groups[g].children;
        for (var i = 0; i < children.length; i++) {
          children[i].style.setProperty(
            '--reveal-delay',
            Math.min(i * 45, 270) + 'ms'
          );
        }
      }

      var allTargets = Array.prototype.slice.call(
        document.querySelectorAll('.reveal, .reveal-group')
      );

      if (!allTargets.length) {
        clearRevealFailsafe();
        return;
      }

      var sweepRaf = 0;

      function sweepReveals() {
        sweepRaf = 0;
        var scrollY = window.scrollY || docEl.scrollTop || 0;
        var vh = window.innerHeight || 800;
        var cutoff = scrollY + vh * 0.96;

        for (var idx = 0; idx < allTargets.length; idx++) {
          var el = allTargets[idx];
          if (!el.classList.contains('in')) {
            var absTop = el.getBoundingClientRect().top + scrollY;
            if (absTop <= cutoff) {
              el.classList.add('in');
            }
          }
        }
      }

      function scheduleSweep() {
        if (!sweepRaf) {
          sweepRaf = requestAnimationFrame(sweepReveals);
        }
      }

      // Immediate synchronous sweep at boot so above-the-fold and pre-scrolled
      // sections never wait on callbacks or frames.
      sweepReveals();

      if ('IntersectionObserver' in window) {
        var revealObserver = new IntersectionObserver(
          function (entries) {
            for (var k = 0; k < entries.length; k++) {
              var entry = entries[k];
              if (entry.isIntersecting || entry.intersectionRatio > 0) {
                entry.target.classList.add('in');
                revealObserver.unobserve(entry.target);
              }
            }
          },
          { rootMargin: '0px 0px -4% 0px', threshold: 0.01 }
        );

        for (var m = 0; m < allTargets.length; m++) {
          if (!allTargets[m].classList.contains('in')) {
            revealObserver.observe(allTargets[m]);
          }
        }
      }

      window.addEventListener('scroll', scheduleSweep, { passive: true });
      window.addEventListener('resize', scheduleSweep, { passive: true });
      window.addEventListener('hashchange', scheduleSweep, { passive: true });
      window.addEventListener('load', sweepReveals, { once: true });

      if (document.fonts && document.fonts.ready) {
        document.fonts.ready.then(scheduleSweep);
      }

      clearRevealFailsafe();
    } catch (err) {
      revealAllImmediately();
    }
  }

  /* ------------------------------------------------------------------------
     Bootstrap
     ------------------------------------------------------------------------ */
  function boot() {
    initAuroraShader();
    initCardTilt();
    initFlowHighlight();
    initScrollMotion();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot, { once: true });
  } else {
    boot();
  }
})();
