// 模块说明：前端应用挂载入口。职责是创建 React 根节点并将 App 组件
// 渲染到 index.html 的 #root 容器，同时引入全局样式。
// 输入输出：无输入；副作用为在 #root 上挂载 <App/>（StrictMode）。
// 依赖：react/react-dom、./App、./styles.css（Tailwind 全局样式）。
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
