"""The graph. Two nodes and one decision:

    model -> (tool_calls? tools : END) -> tools -> model -> ...

Hand-built rather than a prebuilt convenience, because the shape IS the
documentation: everything the agent can do is visible in the dozen lines
below, and nothing depends on a helper whose signature moves between
langgraph releases.

One llm-chat particular: the passthrough ends the model's turn at its FIRST
completed tool call (the trained stop string), so tool_calls always carries
exactly one entry and a task needing N calls makes N round trips through
this loop. The recursion limit in Settings is the harness against a model
that never stops calling.
"""

from __future__ import annotations

from typing import Sequence

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import SystemMessage
from langchain_core.tools import BaseTool
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.prebuilt import ToolNode

from .config import Settings
from .tools import DEFAULT_TOOLS


def build_agent(model: BaseChatModel, tools: Sequence[BaseTool] | None = None,
                system_prompt: str = ""):
    tools = list(tools if tools is not None else DEFAULT_TOOLS)
    bound = model.bind_tools(tools) if tools else model

    def call_model(state: MessagesState) -> dict:
        messages = state["messages"]
        if system_prompt and not isinstance(messages[0], SystemMessage):
            messages = [SystemMessage(content=system_prompt), *messages]
        return {"messages": [bound.invoke(messages)]}

    def route(state: MessagesState) -> str:
        return "tools" if state["messages"][-1].tool_calls else END

    graph = StateGraph(MessagesState)
    graph.add_node("model", call_model)
    graph.add_node("tools", ToolNode(tools))
    graph.add_edge(START, "model")
    graph.add_conditional_edges("model", route, {"tools": "tools", END: END})
    graph.add_edge("tools", "model")
    return graph.compile()


def run_once(agent, prompt: str, settings: Settings, history: list | None = None) -> list:
    """One user turn through the loop; returns the full updated message list."""
    messages = list(history or [])
    messages.append(("user", prompt))
    out = agent.invoke({"messages": messages},
                       config={"recursion_limit": settings.recursion_limit})
    return out["messages"]
