defmodule FloeWeb.ApiJSON do
  def whip(%{sdp: sdp}) do
    %{sdp: sdp, type: "answer"}
  end
end
