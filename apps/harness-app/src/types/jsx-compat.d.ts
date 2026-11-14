import type * as React from "react";

/**
 * React 19 移除了全局 JSX 命名空间，但 @zcode/ui 源码仍直接引用 `JSX.Element`。
 * 应用侧补齐该成员，避免检查共享组件源码时出现 TS2694；仅类型补丁，不影响运行时。
 */
declare global {
  namespace JSX {
    interface Element extends React.JSX.Element {}
  }
}
