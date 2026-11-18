#!/opt/homebrew/bin/python3
"""
AgyHub 图标 —— SDF（有符号距离场）绘制。

## 设计：枢纽 / Hub

三条流畅的臂从一个中心核生长出来，中心最粗、末端收细并微微上翘。
三条臂等角度分布，但**同向弯曲**，形成旋转的动感 ——
像水车、像风车、也像设备围绕中枢转。

hub 的本意是「轴心 / 汇聚点」：所有设备连到这一个小点上，
由它接进 Antigravity。所以图形必须是「一个核 + 向外辐射」，
而不是 Antigravity 那个封闭的 A。

色彩沿用 Antigravity 的家族色（深蓝 → 青 → 珊瑚橙），
但走向反过来：它是左上蓝右下橙的斜向渐变，
这里用「中心深、外圈亮」的辐射渐变，结构上完全不同。

## 为什么用 SDF

第一版用「各形状分别画好再叠 alpha」，结果中心球和三条臂之间
出现明显接缝（因为后画的盖住先画的，颜色不连续）。

SDF 的正确做法是：所有形状先转成「到形状表面的有符号距离」，
再用 **smin（smooth minimum）** 平滑合并成一个整体场，
最后一次性由这个场生成 alpha 和颜色。
这样形状之间是**真正融合**的，过渡处自然形成圆角，没有任何接缝。

smin 是图形学里做有机融合的标准技巧。
"""

import os
import sys
import math
import numpy as np
from PIL import Image

# ============================================================
# 配置
# ============================================================

SIZE = 1024
BG = np.array([250, 249, 245], dtype=np.float32)

# Antigravity 家族色
C_DEEP = np.array([32, 104, 224], dtype=np.float32)    # 蓝（中心，不压到发黑）
C_BLUE = np.array([48, 132, 240], dtype=np.float32)    # 亮蓝
C_CYAN = np.array([76, 196, 230], dtype=np.float32)    # 青
C_CORAL = np.array([255, 146, 96], dtype=np.float32)   # 珊瑚橙（提亮，避免在暖白底上发闷）

# 几何参数（归一化坐标，画布半径 = 1）
R_CORE = 0.245        # 中心核半径
ARM_REACH = 0.790     # 臂末端到中心的距离
W_ROOT = 0.190        # 臂根部半宽
W_TIP = 0.108         # 臂末端半宽（加粗→圆润收尾，不是尖的）
BEND = 0.115          # 弯曲强度（小一点更均衡，大了重心会偏）
SMOOTH_K = 0.170      # smin 融合长度（越大越"融"）


def smoothstep(e0, e1, x):
    t = np.clip((x - e0) / (e1 - e0), 0, 1)
    return t * t * (3 - 2 * t)


def smin(a, b, k):
    """平滑最小：两个 SDF 融合处形成自然圆角。"""
    h = np.maximum(k - np.abs(a - b), 0.0) / k
    return np.minimum(a, b) - h * h * (k * 0.25)


def bezier(p0, p1, p2, n):
    ts = np.linspace(0, 1, n)
    pts = ((1 - ts) ** 2)[:, None] * p0 + \
          (2 * (1 - ts) * ts)[:, None] * p1 + \
          (ts ** 2)[:, None] * p2
    return pts, ts


def arm_sdf(nx, ny, ang_deg, bend_sign=1.0, reach=ARM_REACH,
            w_root=W_ROOT, w_tip=W_TIP, bend=BEND, n_seg=72):
    """
    一条臂的有符号距离场。

    臂的中轴是一条二次贝塞尔曲线（带侧向弯曲），
    宽度沿曲线从 w_root 线性收到 w_tip。
    对每个像素求「到中轴的距离 - 该处半宽」，就是这个像素到臂表面的距离。
    """
    a = math.radians(ang_deg)
    p0 = np.array([0.0, 0.0])
    p2 = np.array([math.cos(a) * reach, math.sin(a) * reach])
    perp = a + math.pi / 2
    p1 = (p0 + p2) * 0.5 + np.array([
        math.cos(perp) * bend * bend_sign,
        math.sin(perp) * bend * bend_sign,
    ])

    pts, ts = bezier(p0, p1, p2, n_seg)

    # 逐段求距离，取最小
    best = np.full(nx.shape, 1e9, dtype=np.float32)
    best_t = np.zeros(nx.shape, dtype=np.float32)

    for i in range(len(pts) - 1):
        q0 = pts[i]
        q1 = pts[i + 1]
        seg = q1 - q0
        seg_len2 = float(seg @ seg)
        if seg_len2 < 1e-12:
            continue

        wx = nx - q0[0]
        wy = ny - q0[1]
        t = np.clip((wx * seg[0] + wy * seg[1]) / seg_len2, 0, 1)

        px = q0[0] + seg[0] * t
        py = q0[1] + seg[1] * t
        d = np.sqrt((nx - px) ** 2 + (ny - py) ** 2)

        upd = d < best
        best = np.where(upd, d, best)
        # 沿整条臂的参数（0=根部，1=末端）
        best_t = np.where(upd, (i + t) / (len(pts) - 1), best_t)

    # 宽度沿曲线收细，用 0.55 次幂让收细更自然（根部保持厚实）
    half_w = w_root + (w_tip - w_root) * (best_t ** 0.55)
    return best - half_w, best_t


def build_field(W):
    """
    构建整体距离场与参数场。
    返回 (sdf, dist_from_center)，坐标归一化到 [-1,1]。
    """
    yy, xx = np.mgrid[0:W, 0:W].astype(np.float32)
    nx = (xx - W / 2.0) / (W / 2.0)
    ny = (yy - W / 2.0) / (W / 2.0)

    # 中心核
    d_center = np.sqrt(nx ** 2 + ny ** 2)
    sdf = d_center - R_CORE

    # 三条臂，同向弯曲（bend_sign 全为 +1 → 顺时针旋转感）
    for ang in (-90.0, 30.0, 150.0):
        d_arm, _ = arm_sdf(nx, ny, ang, bend_sign=1.0)
        sdf = smin(sdf, d_arm, SMOOTH_K)

    return sdf, d_center


def build_colors(d_center, W):
    """
    辐射渐变：中心深蓝 → 亮蓝 → 青 → 【亮白过渡】 → 珊瑚橙。

    ⚠️ 关键在中间那个「亮白过渡」色标。

    直接把青插值到橙，RGB 上会经过灰（因为两者明度接近但色相相差 180°），
    画面上就是末端那块发闷的脏棕色。这是 RGB 线性插值的固有问题。

    解法：在青和橙之间插一个接近白的浅色标，让插值路径绕开灰色区。
    视觉上还多了一点「光从末端透出来」的质感，反而更好看。
    """
    yy, xx = np.mgrid[0:W, 0:W].astype(np.float32)
    nx = (xx - W / 2.0) / (W / 2.0)
    ny = (yy - W / 2.0) / (W / 2.0)

    r = np.clip(d_center / 0.95, 0, 1)
    # 斜向微调：右下更暖、左上更冷（Antigravity 原图标的方向感）
    bias = 0.10 * (nx - ny) * 0.7071
    t = np.clip(r + bias, 0, 1)

    # 色标（位置, 颜色）
    stops = [
        (0.00, C_DEEP),                                        # 深蓝
        (0.32, C_BLUE),                                        # 亮蓝
        (0.58, C_CYAN),                                        # 青
        (0.76, np.array([205, 242, 250], dtype=np.float32)),   # 亮白过渡（绕开灰）
        (0.90, np.array([255, 190, 150], dtype=np.float32)),    # 浅橙
        (1.00, C_CORAL),                                       # 珊瑚橙
    ]

    c = np.zeros((W, W, 3), dtype=np.float32)
    c[:] = stops[0][1]
    for i in range(len(stops) - 1):
        p0, c0 = stops[i]
        p1, c1 = stops[i + 1]
        u = smoothstep(p0, p1, t)
        c = c * (1 - u[..., None]) + c1 * u[..., None]

    # 轻微立体感：光从左上打来，幅度压到很小
    # （幅度大了会把橙色压脏，这个值试过多次）
    light = 1.0 - 0.055 * (nx * 0.5 + ny * 0.5)
    c = c * light[..., None]

    return np.clip(c, 0, 255)


def squircle_alpha(W, n=5.0):
    """Apple 风格圆角遮罩（超级椭圆，n≈5 接近系统曲率）。"""
    yy, xx = np.mgrid[0:W, 0:W].astype(np.float32)
    nx = (xx - W / 2.0) / (W / 2.0)
    ny = (yy - W / 2.0) / (W / 2.0)
    val = np.abs(nx) ** n + np.abs(ny) ** n
    aa = 0.010
    return smoothstep(1.0 + aa, 1.0 - aa, val)


def capsule_sdf(nx, ny, p0, p1, w0, w1):
    """
    变宽度的胶囊（线段）距离场。

    用于画 A 的两条腿：从顶点到末端，半宽从 w0 渐变到 w1，
    做出「顶点厚、末端细」的书法笔触感 —— 这是 Antigravity
    那个 A 的关键特征，等宽线段画出来会很呆板。
    """
    ax, ay = p0
    bx, by = p1
    dx, dy = bx - ax, by - ay
    seg_len2 = dx * dx + dy * dy
    t = np.clip(((nx - ax) * dx + (ny - ay) * dy) / seg_len2, 0, 1)

    # 到线段的最近点
    px = ax + dx * t
    py = ay + dy * t
    dist = np.sqrt((nx - px) ** 2 + (ny - py) ** 2)

    # 半宽沿 t 线性过渡
    half_w = w0 + (w1 - w0) * t
    return dist - half_w


def ellipse_ring_sdf(nx, ny, cx, cy, rx, ry, w):
    """
    椭圆环距离场（近似）。

    注意这不是严格的解析距离，而是「归一化椭圆距离 × 平均半径」的近似。
    对图标这种视觉用途足够精确；严格解析式需要解四次方程，不值得。
    """
    ax = (nx - cx) / rx
    ay = (ny - cy) / ry
    d_norm = np.sqrt(ax ** 2 + ay ** 2)
    # 转回近似实际距离
    d = (d_norm - 1.0) * ((rx + ry) / 2.0)
    return np.abs(d) - w


def polyline_sdf(nx, ny, pts, widths):
    """
    变宽度折线（多边形链）的距离场。

    pts     —— 采样点列表 [(x,y), ...]
    widths  —— 与 pts 等长的半宽列表

    用于画贝塞尔托弧：先采样成折线，再逐段求距离取最小。
    比解析椭圆灵活得多 —— 椭圆在某点的切线方向是受参数约束的，
    而折线可以精确控制两端切线与 A 的腿对齐。
    """
    best = np.full(nx.shape, 1e9, dtype=np.float32)
    best_t = np.zeros(nx.shape, dtype=np.float32)
    n = len(pts) - 1

    for i in range(n):
        ax, ay = pts[i]
        bx, by = pts[i + 1]
        dx, dy = bx - ax, by - ay
        seg_len2 = dx * dx + dy * dy
        if seg_len2 < 1e-12:
            continue

        t = np.clip(((nx - ax) * dx + (ny - ay) * dy) / seg_len2, 0, 1)
        px = ax + dx * t
        py = ay + dy * t
        dist = np.sqrt((nx - px) ** 2 + (ny - py) ** 2)

        half_w = widths[i] + (widths[i + 1] - widths[i]) * t

        upd = dist < best
        best = np.where(upd, dist, best)
        best_t = np.where(upd, (i + t) / n, best_t)

    # 用逐点半宽插值得到该处的半宽
    w_at = np.interp(best_t, np.linspace(0, 1, len(widths)), widths)
    return best - w_at


def cubic_bezier(p0, p1, p2, p3, steps=120):
    """三次贝塞尔采样。"""
    out = []
    for i in range(steps + 1):
        t = i / steps
        mt = 1 - t
        x = (mt ** 3) * p0[0] + 3 * (mt ** 2) * t * p1[0] + 3 * mt * (t ** 2) * p2[0] + (t ** 3) * p3[0]
        y = (mt ** 3) * p0[1] + 3 * (mt ** 2) * t * p1[1] + 3 * mt * (t ** 2) * p2[1] + (t ** 3) * p3[1]
        out.append((x, y))
    return out


def build_a_curve(steps=260):
    """
    生成「一笔画成」的 A 形曲线采样点，形状按 Antigravity 实测轮廓反推。

    ## 实测数据（对 Antigravity 图标做饱和度分割后逐行统计）

        高度%    宽度占最大宽%
         0        5      ← 顶点极尖
         8       28
        24       43
        48       57
        72       72
        96      100      ← 底部最宽

    宽高比 1.086（略宽于高）。

    ## 由此得到的结论

    这是一个**底宽顶尖的尖圆顶**（像山峰），不是馒头形。
    顶点宽度只有整体宽度的 5%，几乎是个尖 —— 之前做出来顶部宽 40%+，
    难怪看着又钝又闷。

    ## 构造

    右半段一条三次贝塞尔，从顶点(0, top) 到底部右端(rx, bottom)：
      · 顶点切线**水平** → P1 与顶点等高，保证 C1 连续、顶部圆润不尖角
      · 底部切线**接近竖直** → P2 放在底端正上方，让腿是"垂直落下"的姿态
    """
    # 归一化几何（画布半径 = 1）
    top_y = -0.700          # 顶点
    bot_y = 0.470           # 底部两端
    rx = 0.640              # 底部半宽（宽高比 ≈1.09，对齐参考图）
    # 顶点处切线水平 → 控制点与顶点等高，横向拉出一小段
    # ---- 控制点位置决定腰的宽窄（试错记录）----
    #
    # 想让中段窄，P2 必须**贴近底部**（c2y 接近 bot_y），
    # 且 x 略小于 rx。这样曲线从"底部拐角"才向外走，
    # 之前的行程基本是竖直的 → 窄腰。
    #
    # 反过来，把 c2y 提到顶部附近会让曲线**更早展开**，腰反而更宽。
    # 我最初凭直觉上提，实测腰宽了 6~40%，是个反直觉的坑。
    c1x = 0.135          # 顶点切线：短一些，顶点更尖
    c2y = bot_y - 0.330  # 底部切线控制点：贴近底部
    c2x = rx - 0.190     # 内收更多，腰更窄

    right = cubic_bezier(
        (0.0, top_y),          # P0 顶点
        (c1x, top_y),          # P1 顶点切线方向（水平）
        (c2x, c2y),            # P2 底部切线方向（竖直上方）
        (rx, bot_y),           # P3 底端
        steps=steps,
    )
    # 左半：镜像后反向，拼成从左底→顶→右底的完整路径
    left = [(-x, y) for (x, y) in right]
    left.reverse()
    return left + right[1:]


def a_width_at(u):
    """
    沿 A 曲线的半宽。

    u = 0 在左腿末端，0.5 在顶点，1 在右腿末端。
    两端细、顶点略厚 —— Antigravity 的笔画就是这个走向：
    腿末端利落收细，顶部因为曲率大显得饱满。
    """
    # 参考图里顶点比腿**明显粗**（顶部曲率大，视觉上更饱满），
    # 腿向下逐渐收细，末端是圆头。
    # 用 u 的单调函数而不是 sin 对称函数：两端粗细不同。
    bump = math.sin(math.pi * u) ** 0.65
    return 0.058 + 0.098 * bump


GAP_DEG = 21.0   # 底弧缺口半角（度）


def render(size=SIZE):
    """
    App 图标：**人字形 A + 底部闭环**。

    ## 为什么是这个形态

    之前的版本是一个「环包着中心点」，抽象是够抽象，但和 AgyHub 这个名字
    毫无关联 —— 用户原话：「这个好像和 agy hub 没有联系？没有 A 的那种样式？」

    所以必须让「A」出现。但直接照抄 Antigravity 的 A 有两个问题：
      1. 会被当成 Antigravity 本身，不是我们的 App
      2. 少了一个「hub」的意思

    ## 解法：A 的两条腿往下延伸，在底部连成一个闭环

      · 上半部分是人字形 A  → 一眼认出是 Antigravity 家族
      · 下半部分闭环        → 设备互相连通，就是 hub 的意思
      · 中间的负空间        → 天然形成一个"汇聚口"，指向中心

    这样 A 和 hub 是**同一个图形**，不是两个概念硬拼。

    ## 笔触

    借鉴 Antigravity 的书法感：顶点厚、末端细（capsule_sdf 做变宽度）。
    等宽线段画出来会很呆板，像几何练习。
    """
    W = size
    W2 = W / 2.0

    yy, xx = np.mgrid[0:W, 0:W].astype(np.float32)
    nx = (xx - W2) / W2
    ny = (yy - W2) / W2

    # ---- 底色：斜向渐变（Antigravity 家族色）----
    t = np.clip((nx + ny) / 2.0 * 0.5 + 0.5, 0, 1)
    t = smoothstep(0.0, 1.0, t)
    top = np.array([40, 138, 240], dtype=np.float32)      # 亮蓝
    bottom = np.array([20, 66, 180], dtype=np.float32)    # 深蓝
    bg_col = bottom[None, None, :] * (1 - t[..., None]) + top[None, None, :] * t[..., None]

    # 右上角青色调
    glow = np.clip((nx * 0.7 - ny * 0.7) / 2.0 + 0.5, 0, 1) ** 2
    cyan_tint = np.array([66, 198, 234], dtype=np.float32)
    bg_col = bg_col * (1 - glow[..., None] * 0.28) + cyan_tint[None, None, :] * (glow[..., None] * 0.28)

    # ---- 符号：A 字形 + 带节点的横杠 ----
    #
    # ## 为什么最终是这个形态
    #
    # 之前试过「A 的两腿向下闭合成环」，结果整个图形读成**圆角三角形**，
    # A 的人字形特征被吃掉了 —— 想同时兼顾 A 和 hub，反而两头不讨好。
    #
    # 现在拆开：
    #   · 主体就是干净的 A（人字形、顶点圆润、腿末端收细）
    #     → 一眼认出是 Antigravity 的 A
    #   · A 的横杠做成「两端各一个圆点、中间细线」
    #     → 这就是 hub 的连接语义：两个节点由一条线连起来
    #   横杠本来就在 A 的结构里，不是硬加的东西，两者天然统一。
    #
    # 横杠刻意**不顶到腿**（留空隙），这样视觉上它是"悬浮的连接"，
    # 而不是实心三角的一部分 —— 避免整个图形又读成三角形。

    # ---- A：一条连续曲线（不是两条直线拼）----
    a_pts = build_a_curve(steps=260)
    a_ws = [a_width_at(i / (len(a_pts) - 1)) for i in range(len(a_pts))]
    a_shape = polyline_sdf(nx, ny, a_pts, a_ws)

    # ---- 贯穿弧：两端探出并上翘 ----
    #
    # 依据用户手绘草图：A 的中间横着一条**弧线**，两端穿出腿外、末端向上翘。
    #
    # 这一笔同时满足三个意思：
    #   · 是 A 的横杠     → 读作 A，和 Antigravity 有血缘
    #   · 横杆贯通两侧     → 带上 H 的结构感
    #   · 两端外探并上翘   → hub 的连接语义（伸出去接设备）
    #
    # 实现要点：
    #   三次贝塞尔的**端点放在上方、控制点放在下方**，
    #   曲线从端点往下走，视觉上端头就自然翘起来了 —— 不需要额外画钩。
    #   宽度做两头细、中间粗，和 A 的书法笔触呼应。

    # ⚠️ 端点参数踩过的坑：
    #   端点上翘太多 + 端点太细 → 在腿外侧形成两个明显**尖刺**，
    #   看着像衣服上的线头。修法：上翘量减半、端点加粗成圆头。
    # 端点位置按几何算出来，不靠眼估：
    #   在 bar_y=0.150 处，A 曲线的 x 坐标是 0.480（数值求解贝塞尔得到）。
    #   端点取 0.512 = 只探出腿外 0.032，形成很小的圆头。
    #   之前取 0.545（探出 0.065）外加线宽，就变成了"耳朵"挂在腿外。
    # 位置：参考图实心区间是高度 20%~54%（y 从 -0.70 到 0.47 换算）
    #   y = top + (bot-top)*pct  →  20% ≈ -0.30,  54% ≈ +0.13
    # 取中线附近 y≈-0.05，让弧落在 A 的中段
    bar_y = -0.020             # 弧的高度（A 中段）
    tip_y = bar_y - 0.045      # 只轻微上翘，避免变成尖刺/耳朵
    tip_x = 0.628              # 探出腿外 0.227（腿在 0.401）→ 明显穿出，且不顶到边缘
    ctrl_x = 0.330             # 控制点横向位置
    ctrl_y = bar_y + 0.062     # 控制点低于弧心 → 弧微微下垂

    arc_pts = cubic_bezier(
        (-tip_x, tip_y),          # 左端（上翘）
        (-ctrl_x, ctrl_y),        # 左下控制点
        (ctrl_x, ctrl_y),         # 右下控制点
        (tip_x, tip_y),           # 右端（上翘）
        steps=200,
    )

    # 宽度：两端细、中间粗
    n = len(arc_pts)
    arc_w = []
    for i in range(n):
        u = i / (n - 1)
        # 端点保持 0.88 倍（圆头），中间 1.0 —— 收太多会变尖刺
        # 端部收细到 0.62，让弧末端干净地没入腿中，不形成凸起
        k = 0.62 + 0.38 * math.sin(math.pi * u) ** 0.6
        arc_w.append(0.068 * k)

    d_bar = polyline_sdf(nx, ny, arc_pts, arc_w)

    # 与 A 融合。k 取小一点，接合处有轻微过渡但不糊成一团。
    # ---- 让弧线"穿过"A，而不是与它糊成一片 ----
    #
    # 关键：弧和 A 都是白色，直接并集（min/smin）会糊成一个白色团块，
    # 完全看不出"穿过"的意思 —— 用户看到的就是这个结果，两端变成圆球鼓包。
    #
    # 正确做法：在弧周围留一道**细缝**，把 A 在那一带切开。
    # 视觉上就是"弧线浮在 A 前面穿过去"，腿在弧的位置断开。
    #   1. 先做弧的"加粗版"（比弧大 SEAM 圈）
    #   2. 从 A 里减去这个加粗版 → A 在弧经过处出现缺口
    #   3. 再把弧本身并回来 → 弧完整、A 有缝
    #
    # SEAM 要够细（小于弧宽的一半），否则腿会断得太开，看着像两截。
    SEAM = 0.018
    d_bar_fat = d_bar - SEAM       # 负得更多 = 更粗（SDF 里减法=膨胀）

    # A 减去"弧的膨胀体"：max(A, -fat) 是标准的 CSG 差集
    a_cut = np.maximum(a_shape, -d_bar_fat)

    # 再把弧本身并回去
    sdf = np.minimum(a_cut, d_bar)

    aa = 1.6 / W2
    mark = smoothstep(aa, -aa, sdf)

    rgb = bg_col * (1 - mark[..., None]) + np.array([255, 255, 255], dtype=np.float32) * mark[..., None]

    corners = squircle_alpha(W, n=5.0)
    out = np.dstack([np.clip(rgb, 0, 255), corners * 255]).astype(np.uint8)
    return Image.fromarray(out, mode="RGBA")


# ============================================================
# 导出
# ============================================================

def export_icns(icon, out_icns):
    import subprocess, tempfile, shutil
    tmp = tempfile.mkdtemp(prefix="agyset-")
    setdir = os.path.join(tmp, "AppIcon.iconset")
    os.makedirs(setdir)
    specs = [
        (16, "icon_16x16.png"), (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"), (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"), (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"), (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"), (1024, "icon_512x512@2x.png"),
    ]
    for px, name in specs:
        icon.resize((px, px), Image.LANCZOS).save(os.path.join(setdir, name))
    subprocess.run(["iconutil", "-c", "icns", setdir, "-o", out_icns],
                   check=True, capture_output=True)
    shutil.rmtree(tmp, ignore_errors=True)


def render_menubar(size=36):
    """
    专门为菜单栏渲染一版。

    ⚠️ 为什么不直接把 App 图标缩小：
      菜单栏可用高度只有 ~22pt。App 图标缩小后，三条臂之间的缝隙
      会糊成一片，变成一个难以辨认的三角块。
      菜单栏图标必须**专门设计**：更粗的臂、更小的留白、
      更明确的三向剪影，在 22pt 下也要一眼看出「三条向外辐射」。

    ⚠️ 为什么不保留彩色：
      菜单栏图标会被系统强制单色化。彩色图转单色后，
      蓝和橙的明度差会变成深浅不一的灰块，看起来脏。
      正确做法是自己出「纯黑 + alpha」的模板图（template image），
      系统会按深浅色主题自动反色，两边都清楚。
    """
    W = 288   # 8x 于 36，充分超采样
    yy, xx = np.mgrid[0:W, 0:W].astype(np.float32)
    nx = (xx - W / 2.0) / (W / 2.0)
    ny = (yy - W / 2.0) / (W / 2.0)

    # 菜单栏版：核更大、臂更粗短，塞进方形里更饱满
    # 参数取向：臂更长更细、融合更克制。
    # 早期用短粗臂 + 大融合值，22pt 下三条臂糊成一个三角块，
    # 完全看不出「辐射」的意思。菜单栏图标宁可细一点，
    # 也要保住三向剪影的可辨认度。
    r_core = 0.175
    sdf = np.sqrt(nx ** 2 + ny ** 2) - r_core

    arms = (-90.0, 30.0, 150.0)
    for ang in arms:
        d_arm, _ = arm_sdf(nx, ny, ang, bend_sign=1.0,
                           reach=0.815, w_root=0.205, w_tip=0.098,
                           bend=0.055)
        sdf = smin(sdf, d_arm, 0.105)

    aa = 1.6 / (W / 2.0)
    shape = smoothstep(aa, -aa, sdf)

    # 裁到实际范围 + 统一留白，避免图形顶边
    ys, xs = np.where(shape > 0.1)
    if len(xs):
        x0, x1, y0, y1 = int(xs.min()), int(xs.max()), int(ys.min()), int(ys.max())
        cx, cy = (x0 + x1) // 2, (y0 + y1) // 2
        half = int(max(x1 - x0, y1 - y0) / 2 * 1.30)
        half = max(half, 1)
        x0, x1 = max(0, cx - half), min(W, cx + half)
        y0, y1 = max(0, cy - half), min(W, cy + half)
        shape = shape[y0:y1, x0:x1]

    m = Image.fromarray((shape * 255).astype(np.uint8), mode="L")
    m = m.resize((size, size), Image.LANCZOS)

    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(Image.new("RGBA", (size, size), (0, 0, 0, 255)), (0, 0), m)
    return out


def render_menubar_color(size=44):
    """
    彩色菜单栏图标 —— 带底色的圆角小方块。

    ⚠️ 为什么不做纯黑模板图：
      模板图（isTemplate=true）只适合系统内置图标那种极简线条。
      我们这个是「第三方工具」，和 Docker / mixin 那一类站在一起时，
      纯黑剪影反而突兀 —— 用户原话：「和其他图标格格不入」。
      系统菜单栏里彩色小方块图标很常见（Docker、iStat、各种守护进程），
      带底色的小方块在深浅色菜单栏下都稳。

    做法：圆角方块底 + 三叉符号，符号用白色，对比度最高。
    """
    S = size * 8  # 超采样
    yy, xx = np.mgrid[0:S, 0:S].astype(np.float32)
    nx = (xx - S / 2.0) / (S / 2.0)
    ny = (yy - S / 2.0) / (S / 2.0)

    # ---- 底：圆角方块（squircle）----
    # 菜单栏图标不能用大圆角，否则在 18pt 下看起来是个圆点。
    # 用小圆角（n=3.2 比 App 图标更方）保留"方块"识别度。
    n_sq = 3.2
    val = np.abs(nx) ** n_sq + np.abs(ny) ** n_sq
    aa = 1.6 / (S / 2.0)
    tile = smoothstep(1.0 + aa, 1.0 - aa, val)

    # 底色：深蓝→青的斜向渐变（和 App 图标同色系，一眼能认出是同一个 App）
    t = np.clip((nx * 0.5 + ny * 0.5 + 1.0) / 2.0, 0, 1)
    bg_col = (C_DEEP[None, None, :] * (1 - t[..., None]) +
              np.array([40, 150, 235], dtype=np.float32)[None, None, :] * t[..., None])

    # ---- 符号（白色）：A + 贯穿弧 ----
    #
    # 必须和 App 图标**同一个设计**，否则用户在菜单栏认不出 Dock 里那个是谁。
    #
    # 差别只在比例：
    #   · 笔画更粗（小尺寸下细线会消失）
    #   · 留白更少（22pt 里要尽量占满）
    #   · 弧的翘头更短（太小了翘头会糊成一坨）
    #
    # 几何按 1.0 归一化再整体缩放，和 render() 保持一致的拓扑。

    # A：与 App 图标同一条曲线（build_a_curve），只是笔画更粗。
    # 小尺寸下必须共用同一套几何，否则菜单栏和 Dock 里的图标看起来不是同一个。
    a_pts = build_a_curve(steps=200)
    a_ws = [0.098 + 0.115 * (math.sin(math.pi * (i / (len(a_pts) - 1))) ** 0.65)
            for i in range(len(a_pts))]
    a_shape = polyline_sdf(nx, ny, a_pts, a_ws)

    # 贯穿弧：小尺寸下翘头更短、线更粗
    bar_y = -0.020
    tip_y = bar_y - 0.042
    tip_x = 0.588              # 菜单栏版也明显穿出
    ctrl_x = 0.250
    ctrl_y = bar_y + 0.058

    arc_pts = cubic_bezier(
        (-tip_x, tip_y), (-ctrl_x, ctrl_y),
        (ctrl_x, ctrl_y), (tip_x, tip_y),
        steps=140,
    )

    n = len(arc_pts)
    arc_w = []
    for i in range(n):
        u = i / (n - 1)
        k = 0.68 + 0.32 * math.sin(math.pi * u) ** 0.6
        arc_w.append(0.086 * k)

    d_bar = polyline_sdf(nx, ny, arc_pts, arc_w)
    # 同样的"穿过"处理，缝更细（小尺寸下粗缝会让腿断得太明显）
    SEAM = 0.020
    d_bar_fat = d_bar - SEAM
    a_cut = np.maximum(a_shape, -d_bar_fat)
    sdf = np.minimum(a_cut, d_bar)

    aa2 = 1.6 / (S / 2.0)
    mark = smoothstep(aa2, -aa2, sdf)

    # 白符号压在底色上
    rgb = bg_col * (1 - mark[..., None]) + np.array([255, 255, 255], dtype=np.float32) * mark[..., None]

    out = np.dstack([np.clip(rgb, 0, 255), tile * 255]).astype(np.uint8)
    img = Image.fromarray(out, mode="RGBA")
    return img.resize((size, size), Image.LANCZOS)


if __name__ == "__main__":
    outdir = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(outdir, exist_ok=True)

    print("计算 SDF…")
    icon = render(SIZE)
    icon.save(os.path.join(outdir, "Icon-1024.png"))
    print("✓ Icon-1024.png")

    export_icns(icon, os.path.join(outdir, "AppIcon.icns"))
    print("✓ AppIcon.icns")

    for sz in (512, 256, 128, 64, 32, 16):
        icon.resize((sz, sz), Image.LANCZOS).save(
            os.path.join(outdir, f"Icon-{sz}.png"))
    print("✓ 多尺寸")

    # 菜单栏：彩色带底色版（44px = 22pt @2x）
    mb = render_menubar_color(44)
    mb.save(os.path.join(outdir, "MenuIcon.png"))
    print("✓ MenuIcon.png（44px，彩色带底色）")
