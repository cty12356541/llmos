/* ============================================================
   llmos · v4 极致特效引擎
   ──────────────────────────────────────────────────────────
   依赖（本地化 vendor/）：
     three.min.js            — WebGL 流体极光背景
     gsap.min.js + ScrollTrigger.min.js — 滚动叙事 / 视差 / 揭幕
   可选：tsparticles.slim.bundle.min.js — 粒子星云
   ──────────────────────────────────────────────────────────
   模块：
     1. WebGL 流体极光背景（鼠标扰动）
     2. 磁吸光标 + 光晕拖尾
     3. GSAP 滚动叙事（视差 + 文字逐行揭幕）
     4. 玻璃卡光泽扫过 + 3D 倾斜
     5. 轻量粒子星云（Canvas，鼠标推开）
   全部遵守 prefers-reduced-motion（降级为静态）
   ============================================================ */
(function () {
  "use strict";

  var REDUCE =
    window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var FINE = window.matchMedia && window.matchMedia("(pointer: fine)").matches;

  var HAS_THREE = typeof window.THREE !== "undefined";
  var HAS_GSAP = typeof window.gsap !== "undefined";
  var HAS_ST = HAS_GSAP && typeof window.ScrollTrigger !== "undefined";

  if (HAS_ST) gsap.registerPlugin(ScrollTrigger);

  /* ============================================================
     0. CSS 极光降级（WebGL 不可用时）
     ──────────────────────────────────────────────────────────
     给 body 加 .css-aurora 类，CSS 用多层径向渐变 + 动画模拟极光
     ============================================================ */
  function enableCssFallback() {
    document.body.classList.add("css-aurora");
    var c = document.getElementById("bg-canvas");
    if (c) c.style.display = "none";
  }

  /* ============================================================
     1. WebGL 流体极光背景
     ──────────────────────────────────────────────────────────
     一个全屏 plane，fragment shader 用 domain-warping noise
     生成缓慢流动的冰蓝光带，深藏青底。鼠标位置轻微扰动光带相位。
     ============================================================ */
  function initAuroraBg() {
    if (REDUCE) { enableCssFallback(); return; }
    if (!HAS_THREE) { enableCssFallback(); return; }

    var canvas = document.getElementById("bg-canvas");
    if (!canvas) { enableCssFallback(); return; }

    /* WebGL 支持检测：失败则降级到 CSS 极光，避免黑屏 */
    var testGl = document.createElement("canvas").getContext("webgl") ||
                 document.createElement("canvas").getContext("experimental-webgl");
    if (!testGl) { enableCssFallback(); return; }

    var renderer;
    try {
      renderer = new THREE.WebGLRenderer({
        canvas: canvas,
        antialias: false,
        alpha: true,
        powerPreference: "high-performance"
      });
    } catch (e) {
      enableCssFallback();
      return;
    }
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, 1.5));
    renderer.setSize(window.innerWidth, window.innerHeight);

    var scene = new THREE.Scene();
    var camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0, 1);

    /* shader uniform：时间、鼠标、分辨率 */
    var uniforms = {
      uTime: { value: 0 },
      uMouse: { value: new THREE.Vector2(0.5, 0.5) },
      uRes: { value: new THREE.Vector2(window.innerWidth, window.innerHeight) }
    };

    /* fragment shader：分形噪声 + domain warping，生成流动极光
       色调严格守 DESIGN.md：深藏青底 + 冰蓝光带（单色家族） */
    var fragmentShader = [
      "precision highp float;",
      "uniform float uTime;",
      "uniform vec2 uMouse;",
      "uniform vec2 uRes;",
      "varying vec2 vUv;",

      /* hash + value noise */
      "float hash(vec2 p){ return fract(sin(dot(p,vec2(127.1,311.7)))*43758.5453); }",
      "float noise(vec2 p){",
      "  vec2 i=floor(p), f=fract(p);",
      "  vec2 u=f*f*(3.0-2.0*f);",
      "  return mix(mix(hash(i+vec2(0,0)),hash(i+vec2(1,0)),u.x),",
      "             mix(hash(i+vec2(0,1)),hash(i+vec2(1,1)),u.x),u.y);",
      "}",
      /* fbm：叠加多 octave */
      "float fbm(vec2 p){",
      "  float v=0.0, a=0.5;",
      "  for(int i=0;i<5;i++){ v+=a*noise(p); p*=2.0; a*=0.5; }",
      "  return v;",
      "}",

      "void main(){",
      "  vec2 uv=vUv;",
      "  vec2 p=uv*2.0-1.0;",
      "  p.x*=uRes.x/uRes.y;",
      "  float t=uTime*0.045;",
      "  /* 鼠标轻微扰动相位 */",
      "  vec2 m=(uMouse-0.5)*0.6;",
      "  p+=m;",

      "  /* domain warping：双层位移产生流动光带 */",
      "  vec2 q=vec2(fbm(p+t),fbm(p- t+vec2(5.2,1.3)));",
      "  vec2 r=vec2(fbm(p+q+t*0.8+vec2(1.7,9.2)),fbm(p+q-t*0.6+vec2(8.3,2.8)));",
      "  float f=fbm(p+r);",

      "  /* 底色：深藏青 */",
      "  vec3 base=vec3(0.012,0.027,0.063);",
      "  /* 极光光带：冰蓝 #7FC8FF ~ 钢蓝，单色家族 */",
      "  vec3 aurora=vec3(0.10,0.32,0.55)*pow(f,1.6);",
      "  aurora+=vec3(0.5,0.78,1.0)*pow(max(0.0,r.x*1.2-f*0.4),3.0)*0.8;",

      "  vec3 col=base+aurora;",
      "  /* 顶部更暗，营造纵深 */",
      "  col*=mix(0.7,1.1,uv.y);",
      "  /* 轻微暗角 */",
      "  float vig=1.0-length(p)*0.18;",
      "  col*=vig;",

      "  gl_FragColor=vec4(col,1.0);",
      "}"
    ].join("\n");

    var vertexShader = [
      "varying vec2 vUv;",
      "void main(){",
      "  vUv=uv;",
      "  gl_Position=vec4(position,1.0);",
      "}"
    ].join("\n");

    var material = new THREE.ShaderMaterial({
      uniforms: uniforms,
      vertexShader: vertexShader,
      fragmentShader: fragmentShader
    });
    var mesh = new THREE.Mesh(new THREE.PlaneGeometry(2, 2), material);
    scene.add(mesh);

    /* 鼠标平滑跟随 */
    var target = { x: 0.5, y: 0.5 };
    window.addEventListener("mousemove", function (e) {
      target.x = e.clientX / window.innerWidth;
      target.y = 1.0 - e.clientY / window.innerHeight;
    }, { passive: true });

    /* 标签页隐藏时暂停，省电 */
    var visible = true;
    document.addEventListener("visibilitychange", function () {
      visible = !document.hidden;
    });

    var last = performance.now();
    function render(now) {
      if (visible) {
        var dt = (now - last) / 1000;
        last = now;
        uniforms.uTime.value += dt;
        /* 鼠标 lerp 平滑 */
        uniforms.uMouse.value.x += (target.x - uniforms.uMouse.value.x) * 0.04;
        uniforms.uMouse.value.y += (target.y - uniforms.uMouse.value.y) * 0.04;
        renderer.render(scene, camera);
      } else {
        last = now;
      }
      requestAnimationFrame(render);
    }
    requestAnimationFrame(render);

    /* resize */
    window.addEventListener("resize", function () {
      renderer.setSize(window.innerWidth, window.innerHeight);
      uniforms.uRes.value.set(window.innerWidth, window.innerHeight);
    }, { passive: true });
  }

  /* ============================================================
     2. 磁吸光标 + 光晕拖尾（桌面端）
     ──────────────────────────────────────────────────────────
     自定义光标：一个小圆点 + 一个大光晕延迟跟随。
     hover 链接/按钮时小点放大，光晕变冰蓝。
     ============================================================ */
  function initMagneticCursor() {
    if (REDUCE || !FINE) return;

    var dot = document.createElement("div");
    dot.className = "cursor-dot";
    var ring = document.createElement("div");
    ring.className = "cursor-ring";
    document.body.appendChild(dot);
    document.body.appendChild(ring);
    document.body.classList.add("has-custom-cursor");

    var mx = window.innerWidth / 2,
      my = window.innerHeight / 2;
    var rx = mx,
      ry = my;
    var hovering = false;

    window.addEventListener("mousemove", function (e) {
      mx = e.clientX;
      my = e.clientY;
      dot.style.transform =
        "translate(" + mx + "px," + my + "px) translate(-50%,-50%)";
    }, { passive: true });

    /* ring 延迟跟随（lerp） */
    function loop() {
      rx += (mx - rx) * 0.18;
      ry += (my - ry) * 0.18;
      var s = hovering ? 1.8 : 1;
      ring.style.transform =
        "translate(" + rx + "px," + ry + "px) translate(-50%,-50%) scale(" + s + ")";
      requestAnimationFrame(loop);
    }
    loop();

    /* hover 联动 */
    var hoverSel =
      "a, button, .btn, .nav-card, .card, .flow-pill, [data-cursor]";
    document.addEventListener("mouseover", function (e) {
      if (e.target.closest(hoverSel)) {
        hovering = true;
        ring.classList.add("hover");
      }
    });
    document.addEventListener("mouseout", function (e) {
      if (e.target.closest(hoverSel)) {
        hovering = false;
        ring.classList.remove("hover");
      }
    });
  }

  /* ============================================================
     3. GSAP 滚动叙事
     ──────────────────────────────────────────────────────────
     - 视差：section 内带 [data-speed] 的元素按速度移动
     - 文字揭幕：标题按行/词 split 后从模糊+位移揭幕
     - 序列入场：reveal-group 用 GSAP timeline 错峰
     ============================================================ */
  function initScrollNarrative() {
    if (!HAS_GSAP || !HAS_ST || REDUCE) return;

    /* 3.1 视差 */
    gsap.utils.toArray("[data-speed]").forEach(function (el) {
      var speed = parseFloat(el.getAttribute("data-speed")) || 1;
      gsap.to(el, {
        y: function () {
          return (1 - speed) * window.innerHeight * 0.3;
        },
        ease: "none",
        scrollTrigger: {
          trigger: el,
          start: "top bottom",
          end: "bottom top",
          scrub: true
        }
      });
    });

    /* 3.2 标题揭幕：把 h1/h2 的文本按词拆分，逐词 fade+blur 入场 */
    gsap.utils
      .toArray(".hero-title, .page-title, .sec-head h2, .ns-quote-line")
      .forEach(function (h) {
        /* 跳过已手动拆分的 */
        if (h.querySelector(".word")) return;
        var text = h.textContent;
        /* 保留 em/span 结构太复杂，这里按字符拆中文/按词拆英文 */
        var chars = Array.from(text);
        h.innerHTML = chars
          .map(function (c) {
            if (c === " " || c === "\n") return c;
            return '<span class="word">' + c + "</span>";
          })
          .join("");
        var words = h.querySelectorAll(".word");
        gsap.set(words, { opacity: 0, y: "0.3em", filter: "blur(8px)" });
        gsap.to(words, {
          opacity: 1,
          y: 0,
          filter: "blur(0px)",
          duration: 0.6,
          ease: "power3.out",
          stagger: 0.03,
          scrollTrigger: {
            trigger: h,
            start: "top 85%",
            toggleActions: "play none none none"
          }
        });
      });

    /* 3.3 reveal-group 增强：用 GSAP 错峰 */
    gsap.utils.toArray(".reveal-group").forEach(function (group) {
      var children = group.children;
      gsap.from(children, {
        opacity: 0,
        y: 28,
        duration: 0.7,
        ease: "power2.out",
        stagger: 0.08,
        scrollTrigger: {
          trigger: group,
          start: "top 82%",
          toggleActions: "play none none none"
        }
      });
    });

    /* 3.4 单个 reveal 元素 */
    gsap.utils.toArray(".reveal").forEach(function (el) {
      if (el.classList.contains("reveal-group")) return;
      gsap.from(el, {
        opacity: 0,
        y: 24,
        duration: 0.8,
        ease: "power2.out",
        scrollTrigger: {
          trigger: el,
          start: "top 85%",
          toggleActions: "play none none none"
        }
      });
    });

    ScrollTrigger.refresh();
  }

  /* ============================================================
     4. 玻璃卡光泽扫过 + 3D 倾斜
     ──────────────────────────────────────────────────────────
     hover 卡片时：一道斜向光带从左上掠过 + 卡片随鼠标轻微 3D 倾斜
     ============================================================ */
  function initGlassTilt() {
    if (REDUCE || !FINE) return;

    var sel = ".card, .stat, .nav-card, .abs-card";
    document.querySelectorAll(sel).forEach(function (card) {
      /* 注入光泽层 */
      if (!card.querySelector(".sheen")) {
        var sheen = document.createElement("span");
        sheen.className = "sheen";
        sheen.setAttribute("aria-hidden", "true");
        card.appendChild(sheen);
      }

      card.addEventListener("mousemove", function (e) {
        var r = card.getBoundingClientRect();
        var px = (e.clientX - r.left) / r.width;
        var py = (e.clientY - r.top) / r.height;
        /* 3D 倾斜：最大 ±6deg */
        var rx = (0.5 - py) * 8;
        var ry = (px - 0.5) * 8;
        card.style.transform =
          "perspective(900px) rotateX(" + rx + "deg) rotateY(" + ry + "deg) translateY(-4px)";
        /* 光泽位置 */
        var s = card.querySelector(".sheen");
        if (s) {
          s.style.opacity = "1";
          s.style.background =
            "radial-gradient(circle at " + px * 100 + "% " + py * 100 +
            "%, rgba(255,255,255,0.12), transparent 45%)";
        }
      });

      card.addEventListener("mouseleave", function () {
        card.style.transform = "";
        var s = card.querySelector(".sheen");
        if (s) s.style.opacity = "";
      });
    });
  }

  /* ============================================================
     5. 轻量粒子星云（Canvas，鼠标推开）
     ──────────────────────────────────────────────────────────
     不用 tsParticles，自写 ~120 颗冰蓝星尘，鼠标附近排斥。
     ============================================================ */
  function initParticleField() {
    if (REDUCE) return;
    var canvas = document.getElementById("particles-canvas");
    if (!canvas) return;

    var ctx = canvas.getContext("2d");
    var w, h, particles;
    var mouse = { x: -9999, y: -9999 };

    function resize() {
      w = canvas.width = window.innerWidth;
      h = canvas.height = window.innerHeight;
      var count = Math.min(140, Math.floor((w * h) / 14000));
      particles = [];
      for (var i = 0; i < count; i++) {
        particles.push({
          x: Math.random() * w,
          y: Math.random() * h,
          ox: 0,
          oy: 0,
          vx: (Math.random() - 0.5) * 0.15,
          vy: (Math.random() - 0.5) * 0.15,
          r: Math.random() * 1.6 + 0.3,
          a: Math.random() * 0.5 + 0.2
        });
        particles[i].ox = particles[i].x;
        particles[i].oy = particles[i].y;
      }
    }
    resize();
    window.addEventListener("resize", resize, { passive: true });

    window.addEventListener("mousemove", function (e) {
      mouse.x = e.clientX;
      mouse.y = e.clientY;
    }, { passive: true });
    window.addEventListener("mouseleave", function () {
      mouse.x = -9999;
      mouse.y = -9999;
    });

    function draw() {
      ctx.clearRect(0, 0, w, h);
      for (var i = 0; i < particles.length; i++) {
        var p = particles[i];
        /* 漂移 */
        p.x += p.vx;
        p.y += p.vy;
        /* 鼠标排斥 */
        var dx = p.x - mouse.x;
        var dy = p.y - mouse.y;
        var d2 = dx * dx + dy * dy;
        if (d2 < 14400) {
          var d = Math.sqrt(d2) || 1;
          var force = (120 - d) / 120;
          p.x += (dx / d) * force * 2.4;
          p.y += (dy / d) * force * 2.4;
        }
        /* 边界回弹 */
        if (p.x < 0) p.x = w;
        if (p.x > w) p.x = 0;
        if (p.y < 0) p.y = h;
        if (p.y > h) p.y = 0;

        ctx.beginPath();
        ctx.arc(p.x, p.y, p.r, 0, Math.PI * 2);
        ctx.fillStyle = "rgba(127,200,255," + p.a + ")";
        ctx.fill();
      }
      /* 邻近连线（极淡） */
      for (var a = 0; a < particles.length; a++) {
        for (var b = a + 1; b < particles.length; b++) {
          var pa = particles[a], pb = particles[b];
          var ddx = pa.x - pb.x, ddy = pa.y - pb.y;
          var dd = ddx * ddx + ddy * ddy;
          if (dd < 11000) {
            ctx.beginPath();
            ctx.moveTo(pa.x, pa.y);
            ctx.lineTo(pb.x, pb.y);
            ctx.strokeStyle = "rgba(127,200,255," + (0.06 * (1 - dd / 11000)) + ")";
            ctx.lineWidth = 0.5;
            ctx.stroke();
          }
        }
      }
      requestAnimationFrame(draw);
    }
    draw();
  }

  /* ============================================================
     初始化（DOM ready）
     ============================================================ */
  function init() {
    initAuroraBg();
    initParticleField();
    initMagneticCursor();
    initGlassTilt();
    /* GSAP 滚动叙事最后跑，确保 ScrollTrigger 量算正确 */
    initScrollNarrative();
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
