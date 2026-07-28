/* llmos v3 动效引擎：流场背景 + SVG 机制图解
   纪律（与 DESIGN.md / 任务约束对齐）：
   - 零依赖自绘，无 CDN；canvas 限 30fps，页面不可见 / 出视口即暂停
   - reduced-motion：不启动任何循环，流场画一帧静态，图解保持完整静态终态
   - 无 JS：canvas 为空（渐进增强），SVG 图解不带 .dg-live，全元素静止可见 */
(function () {
  "use strict";

  var mq =
    window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)");
  function reduced() {
    return !!(mq && mq.matches);
  }

  var FPS_MS = 1000 / 30;
  var live = []; // { start(), stop(), visible }
  var pageHidden = false;

  function register(inst) {
    live.push(inst);
    return inst;
  }

  document.addEventListener("visibilitychange", function () {
    pageHidden = document.hidden;
    live.forEach(function (i) {
      if (pageHidden) i.stop();
      else if (i.visible) i.start();
    });
  });

  function observe(el, inst) {
    if (!("IntersectionObserver" in window)) {
      inst.visible = true;
      inst.start();
      return;
    }
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (e) {
          inst.visible = e.isIntersecting;
          if (e.isIntersecting && !pageHidden) inst.start();
          else inst.stop();
        });
      },
      { threshold: 0.15 }
    );
    io.observe(el);
  }

  /* ---------------- 流场背景 ---------------- */
  function flowBg(canvas) {
    var ctx = canvas.getContext("2d");
    if (!ctx) return;
    var W = 0,
      H = 0,
      parts = [],
      running = false,
      raf = 0,
      last = 0;

    function resize() {
      var dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      var r = canvas.getBoundingClientRect();
      W = Math.max(1, Math.floor(r.width));
      H = Math.max(1, Math.floor(r.height));
      canvas.width = W * dpr;
      canvas.height = H * dpr;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.fillStyle = "#030710";
      ctx.fillRect(0, 0, W, H);
      var n = Math.round(Math.min(100, Math.max(44, W / 14)));
      parts = [];
      for (var i = 0; i < n; i++)
        parts.push({ x: Math.random() * W, y: Math.random() * H, l: 0 });
    }

    function angle(x, y, t) {
      var s = 0.0016;
      return (
        (Math.sin(x * s * 1.7 + t * 0.00011) +
          Math.cos(y * s * 2.3 - t * 0.00008) +
          Math.sin((x + y) * s * 0.9 + t * 0.00005)) *
        Math.PI
      );
    }

    function step(t) {
      ctx.fillStyle = "rgba(3,7,16,0.06)";
      ctx.fillRect(0, 0, W, H);
      ctx.lineWidth = 1;
      ctx.strokeStyle = "rgba(127,200,255,0.10)";
      ctx.beginPath();
      for (var i = 0; i < parts.length; i++) {
        var p = parts[i];
        var a = angle(p.x, p.y, t);
        var nx = p.x + Math.cos(a) * 1.5;
        var ny = p.y + Math.sin(a) * 1.5;
        ctx.moveTo(p.x, p.y);
        ctx.lineTo(nx, ny);
        p.x = nx;
        p.y = ny;
        p.l++;
        if (p.x < -8 || p.x > W + 8 || p.y < -8 || p.y > H + 8 || p.l > 700) {
          p.x = Math.random() * W;
          p.y = Math.random() * H;
          p.l = 0;
        }
      }
      ctx.stroke();
    }

    function loop(t) {
      if (!running) return;
      raf = requestAnimationFrame(loop);
      if (t - last < FPS_MS) return;
      last = t;
      step(t);
    }

    var inst = register({
      visible: false,
      start: function () {
        if (running || reduced()) return;
        running = true;
        last = 0;
        raf = requestAnimationFrame(loop);
      },
      stop: function () {
        running = false;
        cancelAnimationFrame(raf);
      },
    });

    resize();
    var rT;
    window.addEventListener("resize", function () {
      clearTimeout(rT);
      rT = setTimeout(function () {
        resize();
        if (reduced()) staticFrame();
      }, 180);
    });

    function staticFrame() {
      ctx.fillStyle = "#030710";
      ctx.fillRect(0, 0, W, H);
      for (var k = 0; k < 900; k++) step(0);
    }

    if (reduced()) staticFrame();
    else observe(canvas, inst);

    if (mq && mq.addEventListener)
      mq.addEventListener("change", function () {
        inst.stop();
        if (reduced()) staticFrame();
        else if (inst.visible) inst.start();
      });
  }

  /* ---------------- SVG 图解播放器 ---------------- */
  function player(svg, duration, tick) {
    svg.classList.add("dg-live");
    var active = false,
      raf = 0,
      last = 0,
      t0 = -1;
    function loop(t) {
      if (!active) return;
      raf = requestAnimationFrame(loop);
      if (t - last < FPS_MS) return;
      last = t;
      if (t0 < 0) t0 = t;
      tick(((t - t0) % duration) / duration);
    }
    var inst = register({
      visible: false,
      start: function () {
        if (active || reduced()) return;
        active = true;
        last = 0;
        raf = requestAnimationFrame(loop);
      },
      stop: function () {
        active = false;
        cancelAnimationFrame(raf);
      },
    });
    observe(svg, inst);
  }

  function $(svg, sel) {
    return svg.querySelector(sel);
  }
  function $all(svg, sel) {
    return Array.prototype.slice.call(svg.querySelectorAll(sel));
  }

  function pathWalker(path) {
    var len = path.getTotalLength();
    return function (u) {
      return path.getPointAtLength(Math.max(0, Math.min(1, u)) * len);
    };
  }

  function place(el, pt) {
    el.setAttribute("transform", "translate(" + pt.x + "," + pt.y + ")");
  }

  /* 依次点亮辅助：phase 落在窗口内则加 class */
  function lit(el, phase, from, to, cls) {
    el.classList.toggle(cls || "on", phase >= from && phase < to);
  }

  /* 1. 陷入层数据流（index #trapflow） */
  function initTrapflow(svg) {
    var fwd = pathWalker($(svg, ".dg-fwd"));
    var back = pathWalker($(svg, ".dg-back"));
    var pktF = $(svg, ".dg-pkt-f");
    var pktB = $(svg, ".dg-pkt-b");
    var gates = $all(svg, ".dg-gate");
    var model = $(svg, ".dg-model");
    var backLabel = $(svg, ".dg-back-label");
    var backEdge = $(svg, ".dg-back");
    var FWD_END = 0.56,
      HOLD_END = 0.64,
      BACK_END = 0.94;
    // 四个机制点依次点亮的窗口（位于前进段内）
    var win = [
      [0.05, 0.16],
      [0.17, 0.28],
      [0.29, 0.4],
      [0.41, 0.52],
    ];
    player(svg, 7000, function (ph) {
      gates.forEach(function (g, i) {
        var on =
          (ph >= win[i][0] && ph < win[i][1]) ||
          (i === 1 && ph >= 0.7 && ph < 0.82); // 返程时结算只亮预算扣减
        g.classList.toggle("on", on);
      });
      lit(model, ph, HOLD_END - 0.08, HOLD_END + 0.02);
      var settled = ph >= 0.7 && ph < 0.82;
      backEdge.classList.toggle("hot", settled);
      backLabel.classList.toggle("hot", settled);
      if (ph < FWD_END) {
        place(pktF, fwd(ph / FWD_END));
        pktF.style.display = "block";
      } else pktF.style.display = "none";
      if (ph >= HOLD_END && ph < BACK_END) {
        place(pktB, back((ph - HOLD_END) / (BACK_END - HOLD_END)));
        pktB.style.display = "block";
      } else pktB.style.display = "none";
    });
  }

  /* 2. spawn 三划拨（kernel #spawnflow） */
  function initSpawnflow(svg) {
    var paths = $all(svg, ".dg-stream");
    var walkers = paths.map(pathWalker);
    var pkts = $all(svg, ".dg-pkt-s");
    var labels = $all(svg, ".dg-stream-label");
    var child = $(svg, ".dg-child");
    var win = [
      [0.06, 0.34],
      [0.3, 0.58],
      [0.54, 0.82],
    ];
    player(svg, 6500, function (ph) {
      win.forEach(function (w, i) {
        var on = ph >= w[0] && ph < w[1];
        paths[i].classList.toggle("hot", on);
        labels[i].classList.toggle("hot", on);
        if (on) {
          place(pkts[i], walkers[i]((ph - w[0]) / (w[1] - w[0])));
          pkts[i].style.display = "block";
        } else pkts[i].style.display = "none";
      });
      lit(child, ph, 0.84, 0.97);
    });
  }

  /* 3. 语义原子流（modern #atomflow） */
  function initAtomflow(svg) {
    var lane = pathWalker($(svg, ".dg-lane"));
    var atom = $(svg, ".dg-pkt-a");
    var stages = $all(svg, ".dg-stage");
    var verifyBox = $(svg, ".dg-verify-box");
    var tombBox = $(svg, ".dg-tomb-box");
    var win = [
      [0.04, 0.2],
      [0.2, 0.38],
      [0.38, 0.56],
      [0.56, 0.78],
      [0.78, 0.97],
    ];
    player(svg, 9000, function (ph) {
      stages.forEach(function (s, i) {
        s.classList.toggle("on", ph >= win[i][0] && ph < win[i][1]);
      });
      var u = Math.min(ph / 0.92, 1);
      place(atom, lane(u));
      atom.classList.toggle("dg-atom-verified", ph >= 0.62 && ph < 0.82);
      atom.classList.toggle("dg-atom-tomb", ph >= 0.82);
      verifyBox.classList.toggle("verified", ph >= 0.6 && ph < 0.8);
      tombBox.classList.toggle("tombed", ph >= 0.8);
    });
  }

  /* 4. 八原语环（modern #ring8）：任务序列依次点亮 */
  function initRing8(svg) {
    var prims = $all(svg, ".dg-prim");
    var pulse = $(svg, ".dg-pkt-r");
    var chord = $(svg, ".dg-chord");
    // 一个"深度研究"任务的原语序列（按 data-prim 名索引）
    var seq = [
      "acquire",
      "decompose",
      "transmit",
      "generate",
      "verify",
      "relate",
      "aggregate",
      "forget",
    ];
    var byName = {};
    prims.forEach(function (p) {
      byName[p.getAttribute("data-prim")] = p;
    });
    function center(el) {
      var c = el.querySelector("circle");
      return { x: +c.getAttribute("cx"), y: +c.getAttribute("cy") };
    }
    var n = seq.length;
    var seg = 1 / n;
    player(svg, 9600, function (ph) {
      var idx = Math.floor(ph / seg) % n;
      var u = (ph - idx * seg) / seg;
      prims.forEach(function (p) {
        p.classList.toggle(
          "on",
          p.getAttribute("data-prim") === seq[idx]
        );
      });
      var a = center(byName[seq[idx]]);
      var b = center(byName[seq[(idx + 1) % n]]);
      chord.setAttribute("x1", a.x);
      chord.setAttribute("y1", a.y);
      chord.setAttribute("x2", b.x);
      chord.setAttribute("y2", b.y);
      chord.classList.toggle("on", u < 0.85);
      place(pulse, { x: a.x + (b.x - a.x) * u, y: a.y + (b.y - a.y) * u });
    });
  }

  /* 5. Budget 守恒环（verification #budgetflow） */
  function initBudgetflow(svg) {
    var fwd = pathWalker($(svg, ".dg-fwd"));
    var back = pathWalker($(svg, ".dg-back"));
    var pktF = $(svg, ".dg-pkt-f");
    var pktB = $(svg, ".dg-pkt-b");
    var gates = $all(svg, ".dg-gate");
    var FWD_END = 0.62,
      BACK_END = 0.95;
    var win = [
      [0.04, 0.15],
      [0.16, 0.27],
      [0.28, 0.39],
      [0.4, 0.51],
      [0.52, 0.63],
    ];
    player(svg, 8000, function (ph) {
      gates.forEach(function (g, i) {
        g.classList.toggle("on", ph >= win[i][0] && ph < win[i][1]);
      });
      if (ph < FWD_END) {
        place(pktF, fwd(ph / FWD_END));
        pktF.style.display = "block";
      } else pktF.style.display = "none";
      if (ph >= 0.68 && ph < BACK_END) {
        place(pktB, back((ph - 0.68) / (BACK_END - 0.68)));
        pktB.style.display = "block";
      } else pktB.style.display = "none";
    });
  }

  /* ---------------- 装配 ---------------- */
  function boot() {
    $all(document, "canvas.flowbg").forEach(flowBg);

    var t = document.getElementById("trapflow");
    if (t) initTrapflow(t);
    var s = document.getElementById("spawnflow");
    if (s) initSpawnflow(s);
    var a = document.getElementById("atomflow");
    if (a) initAtomflow(a);
    var r = document.getElementById("ring8");
    if (r) initRing8(r);
    var b = document.getElementById("budgetflow");
    if (b) initBudgetflow(b);

    // 三层架构分隔带：进入视口才让数据点流动（CSS 动画由 .in 门控）
    var dividers = $all(document, ".trap-divider");
    if (dividers.length && "IntersectionObserver" in window && !reduced()) {
      var io = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (e) {
            e.target.classList.toggle("in", e.isIntersecting && !pageHidden);
          });
        },
        { threshold: 0.4 }
      );
      dividers.forEach(function (d) {
        io.observe(d);
      });
    }

    // reduced-motion：不加 .dg-live，图解保持完整静态终态（默认 DOM 即终态）
    if (!reduced()) return;
    $all(document, ".dg-live").forEach(function (el) {
      el.classList.remove("dg-live");
    });
  }

  if (document.readyState === "loading")
    document.addEventListener("DOMContentLoaded", boot);
  else boot();
})();
