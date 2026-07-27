import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { LazyMotion } from "motion/react";
import "./site.css";
import { Site } from "./site";

const loadMotionFeatures = () =>
  import("./motion-features").then((module) => module.default);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <LazyMotion features={loadMotionFeatures} strict>
      <Site />
    </LazyMotion>
  </StrictMode>,
);
